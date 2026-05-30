#!/usr/bin/env python3
"""Athen voice-call runner — single outbound phone call via Pipecat.

This script is launched by Athen as a per-call subprocess. It reads a
JSON config (from FD 3, or --config-file on Windows), places an outbound
Twilio call with Media Streams, wires STT ↔ LLM ↔ TTS via Pipecat, runs
the conversation to completion (or hangup, or timeout), then exits with
a final result event.

Stdout protocol:
    One JSON object per line, flushed immediately. Events:
        {"event": "starting"}
        {"event": "installing_check", "detail": "..."}
        {"event": "ringing", "number": "..."}
        {"event": "answered"}
        {"event": "transcript", "speaker": "agent|user", "text": "...", "ts": ...}
        {"event": "result", "outcome": "...", "duration_s": ..., ...}

Stderr is reserved for diagnostic logging (Pipecat etc.).

Required environment (installed by Athen's runtime install pipeline):
    pipecat-ai[deepgram,elevenlabs,cartesia,twilio,openai,anthropic,google]
    pyngrok
    fastapi
    uvicorn

The script expects to be invoked by
    PYTHONPATH=~/.athen/toolbox/pipecat_env \
    ~/.athen/toolbox/runtimes/python/bin/python pipecat_runner.py \
        --config-fd 3   (or --config-file <path>)
"""

from __future__ import annotations

import argparse
import asyncio
import contextlib
import json
import os
import signal
import socket
import sys
import threading
import time
import traceback
from dataclasses import dataclass, field
from typing import Any

# ---------------------------------------------------------------------------
# stdout protocol
# ---------------------------------------------------------------------------

_START_TIME = time.time()
_TRANSCRIPT: list[dict[str, Any]] = []
_AGENT_LAST_MSG = ""


def emit(event: str, **fields: Any) -> None:
    """Emit one JSON event on stdout, flushed immediately."""
    payload = {"event": event, **fields}
    try:
        print(json.dumps(payload, ensure_ascii=False), flush=True)
    except (BrokenPipeError, ValueError):
        # Parent vanished; nothing useful to do.
        pass


def log(msg: str) -> None:
    """Diagnostic — stderr only."""
    print(msg, file=sys.stderr, flush=True)


def emit_transcript(speaker: str, text: str) -> None:
    """Record + emit a transcript line."""
    global _AGENT_LAST_MSG
    text = (text or "").strip()
    if not text:
        return
    entry = {"speaker": speaker, "text": text, "ts": time.time()}
    _TRANSCRIPT.append(entry)
    if speaker == "agent":
        _AGENT_LAST_MSG = text
    emit("transcript", speaker=speaker, text=text, ts=entry["ts"])


# ---------------------------------------------------------------------------
# config
# ---------------------------------------------------------------------------


@dataclass
class CallConfig:
    number: str
    objective: str
    called_party: str
    voice_persona_prefix: str
    llm: dict[str, Any]
    stt: dict[str, Any]
    tts: dict[str, Any]
    phone: dict[str, Any]
    max_duration_s: int = 600
    public_url: str | None = None
    ngrok_authtoken: str | None = None

    @classmethod
    def from_dict(cls, raw: dict[str, Any]) -> "CallConfig":
        required = ["number", "objective", "llm", "stt", "tts", "phone"]
        missing = [k for k in required if k not in raw]
        if missing:
            raise ValueError(f"config missing required keys: {missing}")
        return cls(
            number=str(raw["number"]),
            objective=str(raw["objective"]),
            called_party=str(raw.get("called_party", "other")),
            voice_persona_prefix=str(raw.get("voice_persona_prefix", "")),
            llm=dict(raw["llm"]),
            stt=dict(raw["stt"]),
            tts=dict(raw["tts"]),
            phone=dict(raw["phone"]),
            max_duration_s=int(raw.get("max_duration_s", 600)),
            public_url=raw.get("public_url"),
            ngrok_authtoken=raw.get("ngrok_authtoken"),
        )


def load_config(args: argparse.Namespace) -> CallConfig:
    if args.config_file:
        with open(args.config_file, "r", encoding="utf-8") as fh:
            raw = json.load(fh)
    else:
        fd = args.config_fd if args.config_fd is not None else 3
        with os.fdopen(fd, "r", encoding="utf-8") as fh:
            raw = json.load(fh)
    return CallConfig.from_dict(raw)


# ---------------------------------------------------------------------------
# system prompt
# ---------------------------------------------------------------------------

_FIXED_VOICE_SUFFIX = (
    "Speak naturally, conversationally. Keep responses under 30 words "
    "unless asked. Do not say 'as an AI' or read URLs. Confirm before "
    "hanging up."
)


def build_system_prompt(cfg: CallConfig) -> str:
    persona = cfg.voice_persona_prefix.strip() or "You are a polite assistant placing a call."
    return f"{persona}\n\nYour task: {cfg.objective}\n\n{_FIXED_VOICE_SUFFIX}"


# ---------------------------------------------------------------------------
# Pipecat service construction
# ---------------------------------------------------------------------------


def build_llm_service(cfg: CallConfig):
    """Construct a Pipecat LLM service from the llm config block."""
    kind = cfg.llm.get("type", "openai_compat").lower()
    api_key = cfg.llm.get("api_key", "")
    model = cfg.llm.get("model")
    if not api_key:
        raise ValueError("llm.api_key is required")
    if kind == "openai_compat":
        from pipecat.services.openai import OpenAILLMService  # type: ignore
        base_url = cfg.llm.get("base_url") or "https://api.openai.com/v1"
        return OpenAILLMService(api_key=api_key, model=model or "gpt-4o-mini", base_url=base_url)
    if kind == "anthropic":
        from pipecat.services.anthropic import AnthropicLLMService  # type: ignore
        return AnthropicLLMService(api_key=api_key, model=model or "claude-3-5-haiku-latest")
    if kind == "google":
        from pipecat.services.google import GoogleLLMService  # type: ignore
        return GoogleLLMService(api_key=api_key, model=model or "gemini-2.0-flash")
    raise ValueError(f"unsupported llm.type: {kind!r}")


def build_stt_service(cfg: CallConfig):
    kind = cfg.stt.get("type", "deepgram").lower()
    api_key = cfg.stt.get("api_key", "")
    if not api_key:
        raise ValueError("stt.api_key is required")
    if kind == "deepgram":
        from pipecat.services.deepgram import DeepgramSTTService  # type: ignore
        kwargs: dict[str, Any] = {"api_key": api_key}
        if cfg.stt.get("model"):
            kwargs["model"] = cfg.stt["model"]
        if cfg.stt.get("language"):
            kwargs["language"] = cfg.stt["language"]
        return DeepgramSTTService(**kwargs)
    raise ValueError(f"unsupported stt.type: {kind!r}")


def build_tts_service(cfg: CallConfig):
    kind = cfg.tts.get("type", "elevenlabs").lower()
    api_key = cfg.tts.get("api_key", "")
    voice_id = cfg.tts.get("voice_id", "")
    if not api_key or not voice_id:
        raise ValueError("tts.api_key and tts.voice_id are required")
    if kind == "elevenlabs":
        from pipecat.services.elevenlabs import ElevenLabsTTSService  # type: ignore
        return ElevenLabsTTSService(api_key=api_key, voice_id=voice_id)
    if kind == "cartesia":
        from pipecat.services.cartesia import CartesiaTTSService  # type: ignore
        return CartesiaTTSService(api_key=api_key, voice_id=voice_id)
    raise ValueError(f"unsupported tts.type: {kind!r}")


# ---------------------------------------------------------------------------
# public URL acquisition
# ---------------------------------------------------------------------------


def _pick_free_port() -> int:
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


@dataclass
class TunnelHandle:
    public_url: str
    teardown: Any = None  # callable or None


def _find_cloudflared() -> str | None:
    """Locate a cloudflared binary. Checks PATH and the usual install dirs."""
    import shutil
    found = shutil.which("cloudflared")
    if found:
        return found
    home = os.path.expanduser("~")
    for cand in (
        os.path.join(home, ".local", "bin", "cloudflared"),
        os.path.join(home, ".athen", "toolbox", "runtimes", "cloudflared", "cloudflared"),
        "/usr/local/bin/cloudflared",
        "/usr/bin/cloudflared",
    ):
        if os.path.isfile(cand) and os.access(cand, os.X_OK):
            return cand
    return None


def _cloudflared_tunnel(binary: str, local_port: int) -> TunnelHandle:
    """Start a cloudflared quick tunnel to http://127.0.0.1:<port>.

    Quick tunnels need no account/token and handle WebSocket upgrades
    cleanly (unlike the ngrok free tier, which fails Twilio's Media
    Streams handshake with error 31920). The public URL is scraped from
    cloudflared's stderr banner.
    """
    import re
    import subprocess

    proc = subprocess.Popen(
        [
            binary,
            "tunnel",
            "--no-autoupdate",
            "--url",
            f"http://127.0.0.1:{local_port}",
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        text=True,
    )

    url: str | None = None
    pat = re.compile(r"https://[-a-z0-9]+\.trycloudflare\.com")
    deadline = time.time() + 30
    assert proc.stderr is not None
    while time.time() < deadline:
        line = proc.stderr.readline()
        if not line:
            if proc.poll() is not None:
                break
            continue
        log(f"cloudflared: {line.rstrip()}")
        m = pat.search(line)
        if m:
            url = m.group(0)
            break

    if not url:
        with contextlib.suppress(Exception):
            proc.terminate()
        raise RuntimeError(
            "cloudflared did not report a tunnel URL within 30s — check the runner log"
        )

    # Drain remaining stderr in the background so the pipe never blocks.
    def _drain() -> None:
        with contextlib.suppress(Exception):
            for line in proc.stderr:  # type: ignore[union-attr]
                log(f"cloudflared: {line.rstrip()}")

    threading.Thread(target=_drain, daemon=True).start()

    def teardown() -> None:
        with contextlib.suppress(Exception):
            proc.terminate()
        with contextlib.suppress(Exception):
            proc.wait(timeout=5)

    return TunnelHandle(public_url=url.rstrip("/"), teardown=teardown)


async def _await_tunnel_ready(health_url: str, timeout_s: float = 25.0) -> None:
    """Poll `health_url` until it returns 200, or `timeout_s` elapses.

    Best-effort: a tunnel that never becomes reachable still falls through
    to the call attempt (which will then fail loudly), rather than hanging
    forever. Logs each attempt so the runner log shows how long the tunnel
    took to come up.
    """
    import urllib.request

    deadline = time.time() + timeout_s
    attempt = 0
    while time.time() < deadline:
        attempt += 1
        try:
            def _probe() -> int:
                req = urllib.request.Request(health_url, method="GET")
                with urllib.request.urlopen(req, timeout=5) as resp:
                    return resp.status

            status = await asyncio.to_thread(_probe)
            if 200 <= status < 400:
                log(f"tunnel ready after {attempt} probe(s) (HTTP {status})")
                return
            log(f"tunnel probe {attempt}: HTTP {status}")
        except Exception as e:  # noqa: BLE001
            log(f"tunnel probe {attempt} not ready: {e}")
        await asyncio.sleep(1.0)
    log(f"tunnel not confirmed ready after {timeout_s:.0f}s — placing call anyway")


def acquire_public_url(cfg: CallConfig, local_port: int) -> TunnelHandle:
    """Return a public URL routing to ws://127.0.0.1:<local_port>.

    Priority: explicit cfg.public_url > cloudflared quick tunnel >
    pyngrok with auth token. cloudflared is preferred over ngrok because
    the ngrok free tier fails Twilio's Media Streams WebSocket handshake
    (error 31920) even though plain HTTP traverses it fine.
    Raises RuntimeError with a user-facing message if none work.
    """
    if cfg.public_url:
        return TunnelHandle(public_url=cfg.public_url.rstrip("/"), teardown=None)

    cloudflared = _find_cloudflared()
    if cloudflared:
        log(f"using cloudflared quick tunnel ({cloudflared})")
        return _cloudflared_tunnel(cloudflared, local_port)

    if not cfg.ngrok_authtoken:
        raise RuntimeError(
            "no public URL available — install cloudflared, or set public_url "
            "or ngrok_authtoken in voice config"
        )
    log("cloudflared not found — falling back to ngrok (WebSocket handshake may fail on free tier)")
    try:
        from pyngrok import conf, ngrok  # type: ignore
    except ImportError as e:
        raise RuntimeError(f"pyngrok not installed: {e}") from e
    conf.get_default().auth_token = cfg.ngrok_authtoken
    tunnel = ngrok.connect(local_port, "http")
    url = tunnel.public_url
    if url.startswith("http://"):
        url = "https://" + url[len("http://"):]

    def teardown() -> None:
        with contextlib.suppress(Exception):
            ngrok.disconnect(tunnel.public_url)
        with contextlib.suppress(Exception):
            ngrok.kill()

    return TunnelHandle(public_url=url.rstrip("/"), teardown=teardown)


# ---------------------------------------------------------------------------
# call orchestration
# ---------------------------------------------------------------------------


@dataclass
class CallState:
    answered: bool = False
    ended: bool = False
    end_reason: str | None = None
    twilio_call_sid: str | None = None
    started_at: float = field(default_factory=time.time)


async def run_call(cfg: CallConfig) -> dict[str, Any]:
    """Place + run the call. Returns the dict that becomes the final result event."""
    from fastapi import FastAPI, WebSocket  # type: ignore
    import uvicorn  # type: ignore

    # Build Pipecat services up front so we fail fast on bad config.
    llm = build_llm_service(cfg)
    stt = build_stt_service(cfg)
    tts = build_tts_service(cfg)
    system_prompt = build_system_prompt(cfg)
    state = CallState()

    # FastAPI server that hosts the Twilio Media Streams WebSocket.
    app = FastAPI()
    local_port = _pick_free_port()
    pipeline_task_holder: dict[str, Any] = {"task": None, "runner": None}

    @app.get("/health")
    async def health() -> Any:  # noqa: ANN401
        # Cheap liveness endpoint used to confirm the public tunnel is
        # actually routable BEFORE we place the Twilio call. trycloudflare
        # tunnels warn "it may take some time to be reachable" — placing
        # the call before then makes Twilio's TwiML fetch hit a dead tunnel
        # and the call drops in seconds.
        from fastapi.responses import Response  # type: ignore
        return Response(content="ok", media_type="text/plain")

    @app.post("/twiml")
    async def twiml(_req: Any = None) -> Any:  # noqa: ANN401
        # Returned to Twilio after the call is answered. Tells Twilio to
        # open a bidirectional Media Stream back to our WebSocket.
        from fastapi.responses import Response  # type: ignore
        public_ws = state.public_ws_url  # set just below
        body = (
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>"
            "<Response><Connect>"
            f"<Stream url=\"{public_ws}\"/>"
            "</Connect></Response>"
        )
        return Response(content=body, media_type="application/xml")

    @app.websocket("/ws")
    async def ws_endpoint(websocket: WebSocket) -> None:
        # Twilio dialled our number — we now wire its Media Stream into a
        # Pipecat pipeline.
        await websocket.accept()
        state.answered = True
        emit("answered")
        await _run_pipeline(websocket, llm, stt, tts, system_prompt, state, pipeline_task_holder)

    # Spin up server, then ngrok, then place call.
    emit("installing_check", detail="validating providers")
    server_config = uvicorn.Config(
        app, host="127.0.0.1", port=local_port, log_level="warning", access_log=False
    )
    server = uvicorn.Server(server_config)
    server_task = asyncio.create_task(server.serve())
    # Wait for the socket to come up.
    for _ in range(30):
        if server.started:
            break
        await asyncio.sleep(0.1)

    tunnel: TunnelHandle | None = None
    try:
        tunnel = acquire_public_url(cfg, local_port)
        public_https = tunnel.public_url
        # Convert https://… → wss://… for the media stream URL.
        if public_https.startswith("https://"):
            public_wss = "wss://" + public_https[len("https://"):]
        elif public_https.startswith("http://"):
            public_wss = "ws://" + public_https[len("http://"):]
        else:
            public_wss = public_https
        state.public_ws_url = public_wss + "/ws"  # type: ignore[attr-defined]
        twiml_url = public_https + "/twiml"

        # Wait for the public tunnel to actually route to our server before
        # dialling. A freshly-created trycloudflare tunnel returns its URL
        # before the edge can reach us; calling Twilio too early makes its
        # TwiML fetch hit a dead tunnel → instant hangup. Poll /health
        # through the PUBLIC url until it answers (or give up after ~25s).
        await _await_tunnel_ready(public_https + "/health")

        # Place the call via Twilio REST.
        from twilio.rest import Client as TwilioClient  # type: ignore
        twilio_client = TwilioClient(cfg.phone["account_sid"], cfg.phone["auth_token"])
        call = await asyncio.to_thread(
            twilio_client.calls.create,
            to=cfg.number,
            from_=cfg.phone["from_number"],
            url=twiml_url,
            method="POST",
        )
        state.twilio_call_sid = call.sid
        emit("ringing", number=cfg.number)

        # Wait for the call to finish, or hit max_duration.
        deadline = state.started_at + cfg.max_duration_s
        while not state.ended:
            if time.time() > deadline:
                state.end_reason = "timeout"
                break
            await asyncio.sleep(0.5)
            # Poll Twilio for the call status if we have a SID.
            if state.twilio_call_sid:
                try:
                    fetched = await asyncio.to_thread(
                        twilio_client.calls(state.twilio_call_sid).fetch
                    )
                    if fetched.status in ("completed", "failed", "no-answer", "canceled", "busy"):
                        state.ended = True
                        state.end_reason = state.end_reason or fetched.status
                except Exception as e:  # noqa: BLE001
                    log(f"twilio status poll failed: {e}")

        # Stop the pipeline if still running.
        task = pipeline_task_holder.get("task")
        if task is not None:
            with contextlib.suppress(Exception):
                await task.cancel()
        runner = pipeline_task_holder.get("runner")
        if runner is not None:
            with contextlib.suppress(Exception):
                await runner.stop_when_done()

        # Try to hang up Twilio if we timed out.
        if state.end_reason == "timeout" and state.twilio_call_sid:
            with contextlib.suppress(Exception):
                await asyncio.to_thread(
                    twilio_client.calls(state.twilio_call_sid).update, status="completed"
                )
    finally:
        if tunnel and tunnel.teardown:
            tunnel.teardown()
        server.should_exit = True
        with contextlib.suppress(Exception):
            await asyncio.wait_for(server_task, timeout=3.0)

    return _build_result(cfg, state)


async def _run_pipeline(
    websocket: Any,
    llm: Any,
    stt: Any,
    tts: Any,
    system_prompt: str,
    state: CallState,
    holder: dict[str, Any],
) -> None:
    """Build + run the Pipecat pipeline for the lifetime of one WebSocket."""
    try:
        from pipecat.pipeline.pipeline import Pipeline  # type: ignore
        from pipecat.pipeline.runner import PipelineRunner  # type: ignore
        from pipecat.pipeline.task import PipelineTask  # type: ignore
        from pipecat.transports.network.fastapi_websocket import (  # type: ignore
            FastAPIWebsocketParams,
            FastAPIWebsocketTransport,
        )
        from pipecat.serializers.twilio import TwilioFrameSerializer  # type: ignore
        from pipecat.frames.frames import (  # type: ignore
            LLMMessagesFrame,
            TextFrame,
            TranscriptionFrame,
            EndFrame,
        )
        from pipecat.processors.frame_processor import FrameDirection, FrameProcessor  # type: ignore
    except ImportError as e:
        log(f"pipecat import failed: {e}")
        state.ended = True
        state.end_reason = "error"
        raise

    serializer = TwilioFrameSerializer(stream_sid="")  # populated when Twilio sends `start`
    transport = FastAPIWebsocketTransport(
        websocket=websocket,
        params=FastAPIWebsocketParams(
            audio_in_enabled=True,
            audio_out_enabled=True,
            add_wav_header=False,
            serializer=serializer,
        ),
    )

    # Transcript tap — intercepts user STT + agent TTS frames and emits.
    class TranscriptTap(FrameProcessor):
        async def process_frame(self, frame: Any, direction: FrameDirection) -> None:
            try:
                if isinstance(frame, TranscriptionFrame) and getattr(frame, "text", None):
                    emit_transcript("user", frame.text)
                elif isinstance(frame, TextFrame) and direction == FrameDirection.DOWNSTREAM:
                    txt = getattr(frame, "text", "") or ""
                    if txt.strip():
                        emit_transcript("agent", txt)
            except Exception as e:  # noqa: BLE001
                log(f"transcript tap error: {e}")
            await self.push_frame(frame, direction)

    tap = TranscriptTap()
    messages = [{"role": "system", "content": system_prompt}]

    # Pipeline shape: phone audio → STT → tap → LLM → TTS → tap → phone audio
    pipeline = Pipeline(
        [
            transport.input(),
            stt,
            tap,
            llm,
            tts,
            tap,
            transport.output(),
        ]
    )
    task = PipelineTask(pipeline)
    holder["task"] = task

    # Seed the LLM with the system prompt + an opening hint to start talking.
    await task.queue_frame(LLMMessagesFrame(messages))

    runner = PipelineRunner(handle_sigint=False)
    holder["runner"] = runner
    try:
        await runner.run(task)
    except Exception as e:  # noqa: BLE001
        log(f"pipeline run error: {e}")
    finally:
        with contextlib.suppress(Exception):
            await task.queue_frame(EndFrame())
        state.ended = True
        state.end_reason = state.end_reason or "completed"


# ---------------------------------------------------------------------------
# result + post-call judge
# ---------------------------------------------------------------------------


_GOODBYE_HINTS = ("goodbye", "bye", "talk soon", "have a good", "take care")


def _classify_hangup(end_reason: str | None) -> str:
    if end_reason == "timeout":
        return "timeout"
    last = _AGENT_LAST_MSG.lower()
    if any(h in last for h in _GOODBYE_HINTS):
        return "completed"
    if end_reason in ("failed", "no-answer", "canceled", "busy"):
        return "failed"
    return "user_hangup"


def _cost_estimate(duration_s: int) -> float:
    # Per-minute aggregate from typical published rates (USD).
    twilio_pm = 0.014
    stt_pm = 0.0058  # Deepgram
    tts_pm = 0.05    # ElevenLabs / Cartesia roughly
    per_minute = twilio_pm + stt_pm + tts_pm
    voice_cost = (duration_s / 60.0) * per_minute
    # LLM flat ~30% on top.
    total = voice_cost * 1.30
    return round(total, 2)


def _post_call_judge_sync(cfg: CallConfig, transcript_text: str) -> tuple[str, str]:
    """Use the configured LLM to classify outcome + write a one-sentence summary.

    Best-effort: any failure falls back to ("unclear", "")."""
    prompt = (
        "You are evaluating a phone call. Given the objective and transcript, "
        "respond with a single JSON object {\"outcome\": one of "
        "[\"booked\", \"info_gathered\", \"completed\", \"failed\", \"unclear\"], "
        "\"summary\": one-sentence summary}.\n\n"
        f"Objective: {cfg.objective}\n\nTranscript:\n{transcript_text}\n\nJSON:"
    )
    try:
        kind = cfg.llm.get("type", "openai_compat").lower()
        if kind == "openai_compat":
            import urllib.request
            base = cfg.llm.get("base_url") or "https://api.openai.com/v1"
            req = urllib.request.Request(
                base.rstrip("/") + "/chat/completions",
                data=json.dumps({
                    "model": cfg.llm.get("model", "gpt-4o-mini"),
                    "messages": [{"role": "user", "content": prompt}],
                    "max_tokens": 200,
                    "temperature": 0.0,
                }).encode("utf-8"),
                headers={
                    "Authorization": f"Bearer {cfg.llm['api_key']}",
                    "Content-Type": "application/json",
                },
                method="POST",
            )
            with urllib.request.urlopen(req, timeout=20) as resp:
                payload = json.loads(resp.read())
            content = payload["choices"][0]["message"]["content"]
            parsed = json.loads(_extract_json(content))
            return (str(parsed.get("outcome", "unclear")), str(parsed.get("summary", "")))
        # For non-openai-compat providers, skip the judge — keep the script tight.
        return ("unclear", "")
    except Exception as e:  # noqa: BLE001
        log(f"post-call judge failed: {e}")
        return ("unclear", "")


def _extract_json(text: str) -> str:
    """Find the first {...} block in the text; return as-is if none."""
    start = text.find("{")
    end = text.rfind("}")
    if start >= 0 and end > start:
        return text[start : end + 1]
    return text


def _build_result(cfg: CallConfig, state: CallState) -> dict[str, Any]:
    duration_s = int(time.time() - state.started_at)
    hangup_outcome = _classify_hangup(state.end_reason)
    transcript_text = "\n".join(f"{t['speaker']}: {t['text']}" for t in _TRANSCRIPT)
    if hangup_outcome in ("timeout", "failed"):
        outcome = hangup_outcome
        summary = (
            f"Call ended ({hangup_outcome}) after {duration_s}s. "
            f"{len(_TRANSCRIPT)} transcript turns captured."
        )
    elif _TRANSCRIPT:
        judged_outcome, judged_summary = _post_call_judge_sync(cfg, transcript_text)
        outcome = judged_outcome
        summary = judged_summary or f"Call ended ({hangup_outcome}) after {duration_s}s."
    else:
        outcome = hangup_outcome
        summary = f"Call ended ({hangup_outcome}) with no transcript."
    return {
        "event": "result",
        "transcript": list(_TRANSCRIPT),
        "outcome": outcome,
        "duration_s": duration_s,
        "cost_estimate_usd": _cost_estimate(duration_s),
        "summary": summary,
    }


# ---------------------------------------------------------------------------
# main
# ---------------------------------------------------------------------------


def _parse_args() -> argparse.Namespace:
    p = argparse.ArgumentParser(description="Athen Pipecat voice-call runner")
    p.add_argument("--config-fd", type=int, default=None,
                   help="Inherited file descriptor with config JSON (default: 3)")
    p.add_argument("--config-file", type=str, default=None,
                   help="Path to a config JSON file (Windows fallback)")
    return p.parse_args()


def _install_signal_handlers(loop: asyncio.AbstractEventLoop) -> None:
    def _on_sig(*_: Any) -> None:
        for task in asyncio.all_tasks(loop):
            task.cancel()
    if sys.platform != "win32":
        for sig in (signal.SIGINT, signal.SIGTERM):
            with contextlib.suppress(Exception):
                loop.add_signal_handler(sig, _on_sig)


async def _main_async(cfg: CallConfig) -> dict[str, Any]:
    return await run_call(cfg)


def main() -> int:
    emit("starting")
    args = _parse_args()
    try:
        cfg = load_config(args)
    except Exception as e:  # noqa: BLE001
        emit("result", outcome="error", error=f"config load failed: {e}",
             duration_s=int(time.time() - _START_TIME))
        return 1

    loop = asyncio.new_event_loop()
    asyncio.set_event_loop(loop)
    _install_signal_handlers(loop)
    try:
        result = loop.run_until_complete(_main_async(cfg))
        emit(**result)
        return 0
    except RuntimeError as e:
        # Public URL acquisition + similar pre-call failures.
        emit("result", outcome="error", error=str(e),
             duration_s=int(time.time() - _START_TIME))
        return 1
    except Exception as e:  # noqa: BLE001
        tb = traceback.format_exc()
        log(tb)
        emit("result", outcome="error", error=str(e),
             duration_s=int(time.time() - _START_TIME))
        return 1
    finally:
        with contextlib.suppress(Exception):
            loop.close()


if __name__ == "__main__":
    sys.exit(main())

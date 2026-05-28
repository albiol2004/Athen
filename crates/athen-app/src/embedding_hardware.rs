//! Hardware-based embedding-model tier recommendation.
//!
//! Inspects RAM, CPU cores, free disk on the data-dir mount, and
//! (on macOS) the Apple Silicon generation, then picks one of three
//! tiers — Light / Standard / HighQuality — to surface as a
//! "Recommended for your hardware" badge in Settings.
//!
//! The OS-touching `system_summary()` is kept thin and the pure
//! `recommend_embedding_tier(&SystemSummary)` function takes a
//! constructed summary so tests stay deterministic.

use serde::Serialize;
use std::path::Path;

/// Embedding-model tier we recommend to the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum EmbeddingTier {
    /// Smallest, lowest-quality model (good fallback for weak machines).
    Light,
    /// Mid-range default that fits most machines.
    Standard,
    /// Largest, best-quality model — only when the box can chew it.
    HighQuality,
}

/// Snapshot of the host hardware used to pick a tier.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemSummary {
    pub ram_gb: u64,
    pub physical_cores: usize,
    pub logical_cores: usize,
    /// e.g. "Apple M2 Pro" / "Intel Core i7-1260P" / "AMD Ryzen 9 5950X".
    pub cpu_brand: String,
    /// True when an Apple M-series chip is detected (macOS only).
    pub apple_silicon: bool,
    /// 1 for M1, 2 for M2, etc., when we can read it from `sysctl`.
    /// `None` on non-macOS or when the generation can't be parsed.
    pub apple_silicon_gen: Option<u8>,
    /// True for WSL (and any future cheap VM detection). VMs/WSL tend
    /// to misreport CPU/RAM, so we downgrade conservatively.
    pub is_vm_or_wsl: bool,
    /// Free space on the disk hosting the configured data dir, in GiB.
    pub free_disk_gb: u64,
    /// The pick from `recommend_embedding_tier` baked in at build time.
    pub recommended_tier: EmbeddingTier,
}

/// Pure tier picker — takes a summary so tests can fake every field.
///
/// Rationale for the thresholds (from the parallel research that
/// produced the picking-menu for embedding models):
///
/// * <2 GiB free disk → no model fits, drop to Light.
/// * <8 GiB RAM or <4 cores → embeddings will swap or starve, Light.
/// * ≥16 GiB RAM + ≥8 cores + ≥5 GiB disk → HighQuality is safe.
/// * Apple Silicon M2+ with ≥5 GiB disk → HighQuality (the NPU/AMX
///   pipeline punches well above its RAM/core class on these chips).
/// * Otherwise Standard.
///
/// VM/WSL detection downgrades one tier unless RAM is generous
/// (≥24 GiB), because virt overhead inflates wall-clock latency and
/// cache misses on the embedding hot path.
pub fn recommend_embedding_tier(summary: &SystemSummary) -> EmbeddingTier {
    let base = match (
        summary.ram_gb,
        summary.physical_cores,
        summary.apple_silicon_gen,
        summary.free_disk_gb,
    ) {
        // Hard floors first — these always force Light regardless of
        // other strengths.
        (_, _, _, d) if d < 2 => EmbeddingTier::Light,
        (r, _, _, _) if r < 8 => EmbeddingTier::Light,
        (_, c, _, _) if c < 4 => EmbeddingTier::Light,
        // Strong x86/ARM box.
        (r, c, _, d) if r >= 16 && c >= 8 && d >= 5 => EmbeddingTier::HighQuality,
        // Apple Silicon M2+ — punches above weight on NPU/AMX.
        (_, _, Some(gen), d) if gen >= 2 && d >= 5 => EmbeddingTier::HighQuality,
        _ => EmbeddingTier::Standard,
    };

    if summary.is_vm_or_wsl && summary.ram_gb < 24 {
        downgrade(base)
    } else {
        base
    }
}

fn downgrade(t: EmbeddingTier) -> EmbeddingTier {
    match t {
        EmbeddingTier::HighQuality => EmbeddingTier::Standard,
        EmbeddingTier::Standard => EmbeddingTier::Light,
        EmbeddingTier::Light => EmbeddingTier::Light,
    }
}

/// Build a `SystemSummary` by inspecting the live host. Reads RAM and
/// CPU info from `sysinfo`, free disk from the mount containing
/// `data_dir` (falls back to root if not found), and (on macOS) the
/// Apple Silicon brand from `sysctl machdep.cpu.brand_string`.
pub fn system_summary(data_dir: &Path) -> SystemSummary {
    use sysinfo::{CpuRefreshKind, MemoryRefreshKind, RefreshKind, System};

    let mut sys = System::new_with_specifics(
        RefreshKind::new()
            .with_memory(MemoryRefreshKind::new().with_ram())
            .with_cpu(CpuRefreshKind::new()),
    );
    sys.refresh_memory();
    sys.refresh_cpu_list(CpuRefreshKind::new());

    let ram_gb = sys.total_memory() / 1024 / 1024 / 1024;
    let logical_cores = sys.cpus().len();
    let physical_cores = sys.physical_core_count().unwrap_or(logical_cores);

    // Default CPU brand from sysinfo (good enough on Linux/Windows).
    let cpu_brand_from_sysinfo = sys
        .cpus()
        .first()
        .map(|c| c.brand().to_string())
        .unwrap_or_default();

    // Apple Silicon: ask sysctl directly — `sysinfo`'s brand string on
    // macOS is sometimes empty or generically "Apple M2", which loses
    // the generation digit we want for the recommendation heuristic.
    let (cpu_brand, apple_silicon, apple_silicon_gen) = detect_apple_silicon(cpu_brand_from_sysinfo);

    let free_disk_gb = free_disk_gb_for(data_dir);
    let is_vm_or_wsl = detect_vm_or_wsl();

    let mut summary = SystemSummary {
        ram_gb,
        physical_cores,
        logical_cores,
        cpu_brand,
        apple_silicon,
        apple_silicon_gen,
        is_vm_or_wsl,
        free_disk_gb,
        recommended_tier: EmbeddingTier::Standard,
    };
    summary.recommended_tier = recommend_embedding_tier(&summary);
    summary
}

#[cfg(target_os = "macos")]
fn detect_apple_silicon(fallback_brand: String) -> (String, bool, Option<u8>) {
    use std::process::Command;
    let out = Command::new("sysctl")
        .args(["-n", "machdep.cpu.brand_string"])
        .output();
    let brand = match out {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if s.is_empty() {
                fallback_brand
            } else {
                s
            }
        }
        _ => fallback_brand,
    };
    let (is_apple, gen) = parse_apple_silicon(&brand);
    (brand, is_apple, gen)
}

#[cfg(not(target_os = "macos"))]
fn detect_apple_silicon(fallback_brand: String) -> (String, bool, Option<u8>) {
    (fallback_brand, false, None)
}

/// Pull the "M<digit>" generation out of a brand string like
/// "Apple M2 Pro" or "Apple M3". Returns `(is_apple_silicon, gen)`.
///
/// Used by `detect_apple_silicon` on macOS only; kept compiled on all
/// targets so the unit tests can exercise the parser everywhere.
#[allow(dead_code)]
fn parse_apple_silicon(brand: &str) -> (bool, Option<u8>) {
    if !brand.contains("Apple M") {
        return (false, None);
    }
    // Find the digit(s) right after the "M".
    let after_m = brand.split("Apple M").nth(1).unwrap_or("");
    let digits: String = after_m.chars().take_while(|c| c.is_ascii_digit()).collect();
    let gen = digits.parse::<u8>().ok();
    (true, gen)
}

fn free_disk_gb_for(data_dir: &Path) -> u64 {
    use sysinfo::Disks;
    let disks = Disks::new_with_refreshed_list();

    // Pick the disk whose mount point is the longest prefix of
    // `data_dir`. Longest-match handles nested mounts (e.g.
    // /home being its own mount under /).
    let best = disks
        .list()
        .iter()
        .filter(|d| data_dir.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().as_os_str().len());

    let chosen = best.or_else(|| {
        // Fallback: root mount.
        disks
            .list()
            .iter()
            .find(|d| d.mount_point() == Path::new("/"))
    });

    chosen
        .map(|d| d.available_space() / 1024 / 1024 / 1024)
        .unwrap_or(0)
}

fn detect_vm_or_wsl() -> bool {
    #[cfg(target_os = "linux")]
    {
        is_wsl::is_wsl()
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(ram: u64, cores: usize, disk: u64) -> SystemSummary {
        SystemSummary {
            ram_gb: ram,
            physical_cores: cores,
            logical_cores: cores,
            cpu_brand: "Test CPU".into(),
            apple_silicon: false,
            apple_silicon_gen: None,
            is_vm_or_wsl: false,
            free_disk_gb: disk,
            recommended_tier: EmbeddingTier::Standard,
        }
    }

    #[test]
    fn recommend_light_on_low_ram() {
        let s = summary(4, 8, 100);
        assert_eq!(recommend_embedding_tier(&s), EmbeddingTier::Light);
    }

    #[test]
    fn recommend_light_on_low_cores() {
        let s = summary(32, 2, 100);
        assert_eq!(recommend_embedding_tier(&s), EmbeddingTier::Light);
    }

    #[test]
    fn recommend_light_on_low_disk() {
        let s = summary(32, 8, 1);
        assert_eq!(recommend_embedding_tier(&s), EmbeddingTier::Light);
    }

    #[test]
    fn recommend_hq_on_strong_machine() {
        let s = summary(32, 12, 100);
        assert_eq!(recommend_embedding_tier(&s), EmbeddingTier::HighQuality);
    }

    #[test]
    fn recommend_hq_on_apple_silicon_m2() {
        let mut s = summary(16, 8, 10);
        s.apple_silicon = true;
        s.apple_silicon_gen = Some(2);
        assert_eq!(recommend_embedding_tier(&s), EmbeddingTier::HighQuality);
    }

    #[test]
    fn recommend_standard_on_midrange() {
        let s = summary(12, 6, 50);
        assert_eq!(recommend_embedding_tier(&s), EmbeddingTier::Standard);
    }

    #[test]
    fn vm_downgrades_one_tier() {
        let mut s = summary(12, 6, 50);
        s.is_vm_or_wsl = true;
        // Was Standard, downgrades to Light.
        assert_eq!(recommend_embedding_tier(&s), EmbeddingTier::Light);
    }

    #[test]
    fn vm_with_generous_ram_does_not_downgrade() {
        let mut s = summary(32, 6, 50);
        s.is_vm_or_wsl = true;
        // 32 GiB RAM >= 24 GiB threshold, keeps Standard.
        assert_eq!(recommend_embedding_tier(&s), EmbeddingTier::Standard);
    }

    #[test]
    fn parse_apple_silicon_brand_variants() {
        assert_eq!(parse_apple_silicon("Apple M1"), (true, Some(1)));
        assert_eq!(parse_apple_silicon("Apple M2 Pro"), (true, Some(2)));
        assert_eq!(parse_apple_silicon("Apple M3 Max"), (true, Some(3)));
        assert_eq!(parse_apple_silicon("Apple M4"), (true, Some(4)));
        assert_eq!(parse_apple_silicon("Intel Core i7-1260P"), (false, None));
        assert_eq!(parse_apple_silicon(""), (false, None));
    }

    /// Smoke test — disabled by default. Run with:
    ///   cargo test -p athen-app embedding_hardware::tests::smoke -- --ignored --nocapture
    #[test]
    #[ignore]
    fn smoke_real_system_summary() {
        let s = system_summary(std::path::Path::new("/tmp"));
        println!("SystemSummary: {:#?}", s);
    }
}

//! Embedding providers for Athen.
//!
//! Converts text into vector embeddings using various backends:
//! local models (Ollama), cloud APIs (OpenAI-compatible), and a
//! keyword fallback (TF-IDF hash projection).

pub mod keyword;
pub mod ollama;
pub mod openai;
pub mod router;

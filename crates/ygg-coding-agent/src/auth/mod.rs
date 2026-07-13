#![allow(missing_docs)]

//! Provider authentication flows for subscription-backed models.
//!
//! Currently this is OpenAI Codex ("Sign in with ChatGPT") OAuth. Everything
//! here lives in the product crate and implements the *public*
//! [`ygg_ai::CredentialResolver`] trait, so the frozen `ygg-ai` crate is not
//! touched.

pub mod codex;

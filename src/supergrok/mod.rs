//! SuperGrok (xAI subscription OAuth) usage through the official Grok Build
//! CLI's `x.ai/billing` ACP extension.
//!
//! ai-usagebar never parses, copies, caches, refreshes, or places Grok tokens
//! in ACP messages. Grok Build owns account selection, OIDC issuer discovery, external auth
//! providers, proxy settings, token rotation, and its `auth.json.lock`; this
//! module only parses the extension's credential-free billing response. Login
//! files are read solely as opaque bytes for a one-way cache-scope digest.
//!
//! Distinct from the `grok` vendor, which reads **prepaid Management API**
//! balance with a management key — SuperGrok is the subscription quota path.

pub mod acp;
pub mod block;
pub mod creds;
pub mod fetch;
pub mod scope;
pub mod token;
pub mod types;
pub mod vendor;

pub use fetch::{FetchOutcome, fetch_snapshot};

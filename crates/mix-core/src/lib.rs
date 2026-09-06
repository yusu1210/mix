mod adapter;
mod error;
mod fs;
mod model;
mod process;
mod service;
mod store;
mod transaction;
mod vault;

pub use adapter::{
    AccountCapture, AccountEnrollment, AccountEnrollmentPlan, Adapter, AdapterDescriptor,
    AdapterRegistry, CapturedAccount, ClaudeAdapter, CodexAdapter, GlobalAccountProjection,
    SwitchProjection,
};
pub use error::{Error, ErrorCode, Result};
pub use model::*;
pub use process::ProcessSpec;
pub use service::MixService;
pub use store::ConfigStore;
pub use transaction::FileTokenRewrite;
pub use vault::{local_vault, CredentialVault, VaultCapability};

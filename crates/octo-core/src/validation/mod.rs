//! Startup validation: the result, the validator interface and the helpers every validator
//! shares. The Subsonic validator and the orchestrator live with their services.

pub mod base_startup_validator;
pub mod i_startup_validator;
pub mod validation_result;

pub use base_startup_validator::{BaseStartupValidator, ExceptionKind};
pub use i_startup_validator::IStartupValidator;
pub use validation_result::{ConsoleColor, ValidationResult};

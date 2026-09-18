//! Shared application identity used by the daemon and desktop client.

#[cfg(debug_assertions)]
pub const APP_NAME: &str = "Mack Debug";
#[cfg(not(debug_assertions))]
pub const APP_NAME: &str = "Mack";

#[cfg(debug_assertions)]
pub const APP_ID: &str = "sh.mack.dev";
#[cfg(not(debug_assertions))]
pub const APP_ID: &str = "sh.mack";

#[cfg(debug_assertions)]
pub const DATA_DIRECTORY_NAME: &str = "Mack Debug";
#[cfg(not(debug_assertions))]
pub const DATA_DIRECTORY_NAME: &str = "Mack";

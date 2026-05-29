pub mod html;
pub mod server;
pub mod state;

pub use server::{DashboardServer, set_process_running_flag};
pub use state::{DashboardState, DashboardStateInner};

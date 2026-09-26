//! Extras (PLAN M11): chat giveaways with weighted entries (§14.1), the engine side of remote
//! mod access through the relay (§12.3, §19), first-run setup state (§15.9), runtime DB
//! backups, session-log retention, and the recordings disk budget (§17.4), desktop
//! notifications for background problems (§16.3), and the practice viewers of rehearsal mode
//! (§17.2).

pub mod giveaway;
pub mod maintenance;
pub mod notify;
pub mod rehearsal;
pub mod remote_mod;
pub mod setup;
mod util;

use se_hub::EngineCtx;

/// Start every extras subsystem. Each one runs in its own tasks and logs its own errors.
pub async fn start(ctx: EngineCtx) -> anyhow::Result<()> {
    notify::start(ctx.clone());
    giveaway::start(ctx.clone());
    maintenance::start(ctx.clone());
    remote_mod::start(ctx.clone());
    rehearsal::start(ctx.clone());
    setup::start(ctx);
    Ok(())
}

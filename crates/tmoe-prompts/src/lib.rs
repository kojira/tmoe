//! tmoe-prompts: 3エージェント (Worker / Supervisor / Observer) の
//! 異なる目的関数を持たせるためのシステムプロンプトを集約する。
//!
//! 各エージェントのプロンプトが似通うと合意平面が縮退して直線になり
//! 平面決定性が失われる (3点が同一直線上)。明確に異なる立場を保つ。

pub const WORKER_SYSTEM: &str = "";
pub const SUPERVISOR_SYSTEM: &str = "";
pub const OBSERVER_SYSTEM: &str = "";

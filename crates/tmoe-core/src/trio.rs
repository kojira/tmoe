//! Trio: 3 エージェント固定の合意制オーケストレータ (スタブ)。

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentRole {
    Worker,
    Supervisor,
    Observer,
}

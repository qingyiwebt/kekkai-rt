#[derive(Clone, Debug)]
pub enum Event {
    Started,
    Stdout(String),
    Stderr(String),
    Finished(Option<i32>),
    TimedOut,
    Failed(String),
}

impl Event {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Stdout(_) => "stdout",
            Self::Stderr(_) => "stderr",
            Self::Finished(_) => "finished",
            Self::TimedOut => "timed_out",
            Self::Failed(_) => "failed",
        }
    }

    pub fn data(&self) -> String {
        serde_json::to_string(&match self {
            Self::Started => serde_json::json!({}),
            Self::Stdout(s) => serde_json::json!({"data":s}),
            Self::Stderr(s) => serde_json::json!({"data":s}),
            Self::Finished(c) => serde_json::json!({"exit_code":c}),
            Self::TimedOut => serde_json::json!({}),
            Self::Failed(e) => serde_json::json!({"error":e}),
        })
        .unwrap()
    }
}

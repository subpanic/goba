//! The single error type of goba, and the exit-code contract (R52, §11.2).

#[derive(Debug)]
pub enum Fail {
    Usage(String),
    Ambiguous(String, Vec<String>),
    NotFound(String),
    Store(String),
    Spawn(String),
    Kill(String),
    Remove(String),
}

impl Fail {
    pub fn code(&self) -> i32 {
        match self {
            Fail::Usage(_) => 1,
            Fail::Ambiguous(..) => 2,
            Fail::NotFound(_) => 3,
            Fail::Store(_) => 4,
            Fail::Spawn(_) => 5,
            Fail::Kill(_) => 6,
            Fail::Remove(_) => 7,
        }
    }

    pub fn msg(&self) -> String {
        match self {
            Fail::Usage(m) => m.clone(),
            Fail::Ambiguous(tok, cands) => format!(
                "ambiguous id '{}': matches {}",
                tok,
                cands.join(", ")
            ),
            Fail::NotFound(m) => m.clone(),
            Fail::Store(m) => m.clone(),
            Fail::Spawn(m) => m.clone(),
            Fail::Kill(m) => m.clone(),
            Fail::Remove(m) => m.clone(),
        }
    }
}

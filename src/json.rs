//! A minimal JSON writer. The schema is closed, so this is cheaper (and smaller) than a parser
//! dependency — and it is the only place that has to know about escaping (R32).

use crate::store::{Disp, Meta, Store};

pub fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn num_or_null(v: Option<i64>) -> String {
    match v {
        Some(v) => v.to_string(),
        None => "null".to_string(),
    }
}

pub fn session_list(store: &Store, rows: &[(Meta, Disp)]) -> String {
    let mut out = String::from("[");
    for (i, (m, d)) in rows.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let (state, code, sig) = match d {
            Disp::Running => ("running", None, None),
            Disp::Exited(c) => ("exited", Some(*c as i64), None),
            Disp::Killed(s) => ("killed", None, Some(*s as i64)),
            Disp::Lost => ("lost", None, None),
        };
        let argv = m
            .argv
            .iter()
            .map(|a| format!("\"{}\"", esc(&a.to_string_lossy())))
            .collect::<Vec<_>>()
            .join(",");
        out.push_str(&format!(
            "{{\"num\":{},\"sid\":\"{}\",\"state\":\"{}\",\"exit_code\":{},\"signal\":{},\
             \"pid\":{},\"pgid\":{},\"started_at\":{},\"ended_at\":{},\"cwd\":\"{}\",\"exe\":\"{}\",\
             \"log_bytes\":{},\"argv\":[{}]}}",
            m.num,
            esc(&m.sid),
            state,
            num_or_null(code),
            num_or_null(sig),
            m.pid,
            m.pgid,
            m.started_at,
            m.ended_at,
            esc(&m.cwd.to_string_lossy()),
            esc(&m.exe.to_string_lossy()),
            store.log_bytes(m),
            argv
        ));
    }
    out.push(']');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes() {
        assert_eq!(esc("a\"b\\c"), "a\\\"b\\\\c");
        assert_eq!(esc("tab\there"), "tab\\there");
        assert_eq!(esc("\u{1}"), "\\u0001");
        assert_eq!(esc("plain /path:ok"), "plain /path:ok");
    }
}

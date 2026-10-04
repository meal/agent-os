//! `--crash-at POINT[:KIND][:N]`: a debug flag that kills the process at an engine crash
//! point, to demonstrate recovery. POINT is a [`CrashPoint`] in kebab case, KIND an effect
//! kind tag, N the 1-based pass of that point (counted per effect kind), 1 by default.

use std::str::FromStr;

use agentos_engine::crash::{CrashHook, CrashPoint};

const KINDS: [&str; 7] = [
    "read_snapshot",
    "apply_patch",
    "run_verification",
    "model_call",
    "list_files",
    "read_file",
    "model_retry",
];

/// The command-line name of `point`, e.g. `after-dispatch`.
pub fn point_name(point: CrashPoint) -> String {
    let mut out = String::new();
    for (i, c) in format!("{point:?}").chars().enumerate() {
        if c.is_ascii_uppercase() && i > 0 {
            out.push('-');
        }
        out.push(c.to_ascii_lowercase());
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrashSpec {
    pub point: CrashPoint,
    pub kind: Option<&'static str>,
    pub nth: usize,
}

impl FromStr for CrashSpec {
    type Err = String;

    fn from_str(s: &str) -> Result<CrashSpec, String> {
        let mut parts = s.split(':');
        let name = parts.next().unwrap_or_default();
        let point = CrashPoint::ALL
            .into_iter()
            .find(|p| point_name(*p) == name)
            .ok_or_else(|| {
                let names: Vec<String> = CrashPoint::ALL.into_iter().map(point_name).collect();
                format!(
                    "unknown crash point {name:?}; expected one of {}",
                    names.join(", ")
                )
            })?;
        let mut spec = CrashSpec {
            point,
            kind: None,
            nth: 1,
        };
        let mut rest: Vec<&str> = parts.collect();
        if let Some(Ok(n)) = rest.last().map(|last| last.parse::<usize>()) {
            if n == 0 {
                return Err("N counts from 1".into());
            }
            spec.nth = n;
            rest.pop();
        }
        match rest.as_slice() {
            [] => {}
            [kind] => {
                spec.kind = Some(KINDS.into_iter().find(|k| k == kind).ok_or_else(|| {
                    format!(
                        "unknown effect kind {kind:?}; expected one of {}",
                        KINDS.join(", ")
                    )
                })?);
            }
            _ => return Err(format!("expected POINT[:KIND][:N], got {s:?}")),
        }
        Ok(spec)
    }
}

impl CrashSpec {
    /// The engine hook that fires on the `nth` pass of the point for the kind.
    pub fn hook(&self) -> CrashHook {
        let (point, kind, occurrence) = (self.point, self.kind, self.nth - 1);
        CrashHook::new(move |p, ctx| {
            p == point && kind.is_none_or(|k| ctx.kind == Some(k)) && ctx.occurrence == occurrence
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_kebab_case_and_round_trip() {
        assert_eq!(
            point_name(CrashPoint::AfterAgentTurnJournaled),
            "after-agent-turn-journaled"
        );
        assert_eq!(
            point_name(CrashPoint::AfterExecuteBeforePublish),
            "after-execute-before-publish"
        );
        for p in CrashPoint::ALL {
            assert_eq!(
                point_name(p).parse::<CrashSpec>().unwrap(),
                CrashSpec {
                    point: p,
                    kind: None,
                    nth: 1
                }
            );
        }
    }

    #[test]
    fn kind_and_nth_are_optional() {
        let spec: CrashSpec = "after-dispatch:apply_patch:2".parse().unwrap();
        assert_eq!(
            spec,
            CrashSpec {
                point: CrashPoint::AfterDispatch,
                kind: Some("apply_patch"),
                nth: 2
            }
        );
        for (s, point, kind) in [
            (
                "after-dispatch:model_call",
                CrashPoint::AfterDispatch,
                "model_call",
            ),
            (
                "after-intent:read_file",
                CrashPoint::AfterIntent,
                "read_file",
            ),
            (
                "after-complete:list_files",
                CrashPoint::AfterComplete,
                "list_files",
            ),
        ] {
            let spec: CrashSpec = s.parse().unwrap();
            assert_eq!(
                spec,
                CrashSpec {
                    point,
                    kind: Some(kind),
                    nth: 1
                },
                "{s}"
            );
        }
        let spec: CrashSpec = "after-intent:3".parse().unwrap();
        assert_eq!(
            spec,
            CrashSpec {
                point: CrashPoint::AfterIntent,
                kind: None,
                nth: 3
            }
        );
        for bad in [
            "",
            "after",
            "after-dispatch:teleport",
            "after-dispatch:apply_patch:0",
            "after-dispatch:1:2",
            "after-dispatch:apply_patch:1:x",
        ] {
            assert!(bad.parse::<CrashSpec>().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn hook_fires_on_the_nth_pass_of_the_kind() {
        let hook = "after-dispatch:apply_patch:2"
            .parse::<CrashSpec>()
            .unwrap()
            .hook();
        assert!(!hook.check(CrashPoint::AfterDispatch, Some("read_snapshot")));
        assert!(!hook.check(CrashPoint::AfterDispatch, Some("apply_patch")));
        assert!(!hook.check(CrashPoint::AfterIntent, Some("apply_patch")));
        assert!(hook.check(CrashPoint::AfterDispatch, Some("apply_patch")));
        let any = "after-complete".parse::<CrashSpec>().unwrap().hook();
        assert!(any.check(CrashPoint::AfterComplete, Some("read_snapshot")));
    }
}

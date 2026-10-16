//! Roles: records in, records out. A role is shown a venture's mandate and
//! recent records and answers with inferred records: a proposal, an ask,
//! or nothing. It never acts and never sees a key.

use serde::{Deserialize, Serialize};

use crate::mandate::{Controller, Mandate};
use crate::record::{Claim, Entry, Payload, Record};

/// What a role is shown.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Context {
    pub venture: String,
    pub mandate: Mandate,
    /// Recent records, oldest first, already cut to the context budget.
    pub entries: Vec<Entry>,
    /// running, asking or paused.
    pub state: String,
    /// Unix seconds.
    pub now: i64,
    /// The backend to ask.
    pub backend: String,
}

/// A prompt for a backend. The warden attaches the key and sends it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InferRequest {
    pub backend: String,
    pub system: String,
    pub user: String,
    pub max_tokens: u32,
}

/// Something that infers over a context.
pub trait Role {
    /// Decide for a venture. `infer` sends a prompt to a backend and
    /// returns its reply; a role may call it as often as it likes.
    fn decide(
        &mut self,
        ctx: &Context,
        infer: &mut dyn FnMut(&InferRequest) -> Result<String, String>,
    ) -> Result<Vec<Record>, String>;
}

/// The role a model plays: one prompt, one reply, parsed into records.
#[derive(Debug, Default)]
pub struct ModelRole;

impl Role for ModelRole {
    fn decide(
        &mut self,
        ctx: &Context,
        infer: &mut dyn FnMut(&InferRequest) -> Result<String, String>,
    ) -> Result<Vec<Record>, String> {
        let reply = infer(&prompt(ctx))?;
        Ok(parse_reply(ctx, &reply))
    }
}

/// Is the venture's controller a classifier (one label, no reason)?
pub fn classifies(mandate: &Mandate) -> bool {
    matches!(mandate.controller, Controller::Classifier { .. })
}

/// Build the prompt for a context.
pub fn prompt(ctx: &Context) -> InferRequest {
    let m = &ctx.mandate;
    let names: Vec<&str> = m.permitted.iter().map(|p| p.name.as_str()).collect();
    let system = if classifies(m) {
        format!(
            "You are the overnight reviewer for a supervised venture. You cannot act; \
             you may only pick one label. Reply with exactly one word and nothing else: \
             nothing, ask, or the name of one permitted action ({}).",
            if names.is_empty() {
                "none are permitted".to_string()
            } else {
                names.join(", ")
            }
        )
    } else {
        format!(
            "You are the overnight reviewer for a supervised venture. You cannot act. \
             You may propose exactly one permitted action, ask the human one question, \
             or do nothing. Everything you say is recorded as inferred and never authorizes \
             anything by itself. Reply with one JSON object on one line and nothing else, \
             in one of these forms:\n\
             {{\"choice\": \"nothing\"}}\n\
             {{\"choice\": \"action\", \"action\": \"<one of: {}>\", \"reason\": \"<one sentence>\"}}\n\
             {{\"choice\": \"ask\", \"question\": \"<one sentence>\", \"action\": \"<permitted name, or omit>\"}}\n\
             An action that takes a payload needs a \"payload\" field too: a string for text, \
             diff or file; an object for json. A human reads the payload before it runs.",
            if names.is_empty() {
                "none are permitted".to_string()
            } else {
                names.join(", ")
            }
        )
    };
    let mut user = String::new();
    user.push_str(&format!("Venture: {}\n", ctx.venture));
    user.push_str(&format!("Purpose: {}\n", m.purpose.trim()));
    user.push_str(&format!("Metric: {}\n", m.metric));
    user.push_str(&format!("State: {}\n", ctx.state));
    user.push_str("Permitted actions:\n");
    if m.permitted.is_empty() {
        user.push_str("  (none)\n");
    }
    for p in &m.permitted {
        user.push_str(&format!(
            "  {}: at most {}, {}{}\n",
            p.name,
            p.cap,
            if p.reversible {
                "reversible"
            } else if p.delegated {
                "irreversible, delegated to you"
            } else {
                "irreversible, needs human approval"
            },
            match p.takes {
                Some(t) => format!(", takes {t}"),
                None => String::new(),
            }
        ));
    }
    if !m.forbidden.is_empty() {
        user.push_str(&format!("Forbidden: {}\n", m.forbidden.join(", ")));
    }
    user.push_str("Recent records, oldest first (number, time, venture, claim, kind, summary):\n");
    if ctx.entries.is_empty() {
        user.push_str("  (none yet)\n");
    }
    for e in &ctx.entries {
        user.push_str("  ");
        user.push_str(&e.line());
        user.push('\n');
    }
    user.push_str(if classifies(m) {
        "Your label:"
    } else {
        "Your choice:"
    });
    InferRequest {
        backend: ctx.backend.clone(),
        system,
        user,
        max_tokens: 1024,
    }
}

/// One reply, in JSON form.
#[derive(Deserialize)]
struct Reply {
    choice: String,
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    question: Option<String>,
    #[serde(default)]
    payload: Option<serde_json::Value>,
}

/// Turn a backend's reply into inferred records. Garbled replies mean
/// nothing; a role cannot make records of any other claim.
pub fn parse_reply(ctx: &Context, reply: &str) -> Vec<Record> {
    let text = reply.trim();
    let permitted = |name: &str| ctx.mandate.permitted(name).is_some();
    let venture = ctx.venture.clone();
    // A payload is kept only in the shape the lever takes; a reply that
    // gives another shape, or one for a lever that takes none, loses it.
    let payload_for = |action: &str, value: Option<&serde_json::Value>| -> Option<Payload> {
        let takes = ctx.mandate.permitted(action)?.takes?;
        Payload::from_value(takes, value?).ok()
    };
    let proposal = |action: &str, reason: String, payload: Option<Payload>| Record::Proposal {
        claim: Claim::Inferred,
        venture: venture.clone(),
        action: action.to_string(),
        reason,
        payload,
    };
    let ask = |action: Option<String>, question: String, payload: Option<Payload>| Record::Ask {
        claim: Claim::Inferred,
        venture: venture.clone(),
        action,
        question,
        refusal: None,
        payload,
    };
    if let Some(json) = text
        .find('{')
        .and_then(|s| text.rfind('}').map(|e| &text[s..=e]))
    {
        if let Ok(r) = serde_json::from_str::<Reply>(json) {
            return match r.choice.trim().to_lowercase().as_str() {
                "action" => match r.action.as_deref().map(str::trim) {
                    Some(a) if permitted(a) => vec![proposal(
                        a,
                        r.reason
                            .unwrap_or_else(|| format!("chosen by {}", ctx.backend)),
                        payload_for(a, r.payload.as_ref()),
                    )],
                    _ => vec![],
                },
                "ask" => {
                    let action = r
                        .action
                        .filter(|a| permitted(a.trim()))
                        .map(|a| a.trim().to_string());
                    let payload = action
                        .as_deref()
                        .and_then(|a| payload_for(a, r.payload.as_ref()));
                    vec![ask(
                        action,
                        r.question
                            .unwrap_or_else(|| format!("{} asked for your attention", ctx.backend)),
                        payload,
                    )]
                }
                _ => vec![],
            };
        }
    }
    let line = text.lines().next().unwrap_or("").trim();
    let word = line
        .trim_matches(|c: char| c == '"' || c == '\'' || c == '.' || c == '`')
        .to_lowercase();
    if word == "nothing" || word.is_empty() {
        return vec![];
    }
    if let Some(rest) = line
        .strip_prefix("ask:")
        .or_else(|| line.strip_prefix("ask"))
    {
        let q = rest.trim();
        return vec![ask(
            None,
            if q.is_empty() {
                format!("{} asked for your attention", ctx.backend)
            } else {
                q.to_string()
            },
            None,
        )];
    }
    let (name, reason) = match line.split_once(':') {
        Some((a, r)) if a.trim().eq_ignore_ascii_case("action") => {
            let r = r.trim();
            match r.split_once("--") {
                Some((n, why)) => (n.trim().to_string(), why.trim().to_string()),
                None => (r.to_string(), String::new()),
            }
        }
        _ => (word.clone(), String::new()),
    };
    if permitted(&name) {
        let reason = if reason.is_empty() {
            format!("chosen by {}", ctx.backend)
        } else {
            reason
        };
        return vec![proposal(&name, reason, None)];
    }
    vec![]
}

/// The newest entries that fit a token budget (about four characters a
/// token), oldest first.
pub fn trim_to_budget(entries: &[Entry], budget_tokens: u32) -> Vec<Entry> {
    let mut chars = 0usize;
    let limit = budget_tokens as usize * 4;
    let mut kept: Vec<Entry> = Vec::new();
    for e in entries.iter().rev() {
        chars += e.line().len() + 1;
        if chars > limit && !kept.is_empty() {
            break;
        }
        kept.push(e.clone());
    }
    kept.reverse();
    kept
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mandate::{Cap, Permitted, Restart};
    use crate::record::GENESIS;
    use std::time::Duration;

    fn ctx(classifier: bool) -> Context {
        let permit = |name: &str, reversible: bool| Permitted {
            name: name.to_string(),
            run: "./x".to_string(),
            cap: Cap {
                count: 2,
                per: Cap::DAY,
            },
            reversible,
            delegated: false,
            takes: None,
        };
        Context {
            venture: "shop".to_string(),
            mandate: Mandate {
                purpose: "Sell more".to_string(),
                metric: "orders".to_string(),
                controller: if classifier {
                    Controller::Classifier { backend: None }
                } else {
                    Controller::Model { backend: None }
                },
                permitted: vec![
                    permit("discount", true),
                    permit("refund", false),
                    Permitted {
                        takes: Some(crate::record::Takes::Text),
                        ..permit("announce", true)
                    },
                ],
                forbidden: vec!["close".to_string()],
                cadence: Duration::from_secs(60),
                context_budget: 100,
                restart: Restart::OnFailure,
            },
            entries: vec![Entry::new(
                1,
                0,
                GENESIS,
                Record::report(Claim::Measured, "shop", "orders", Some(3.0), ""),
            )],
            state: "running".to_string(),
            now: 0,
            backend: "test".to_string(),
        }
    }

    #[test]
    fn prompt_names_the_levers_and_the_records() {
        let p = prompt(&ctx(false));
        assert!(
            p.system.contains("one of: discount, refund"),
            "{}",
            p.system
        );
        assert!(
            p.user.contains("discount: at most 2/day, reversible"),
            "{}",
            p.user
        );
        assert!(p
            .user
            .contains("refund: at most 2/day, irreversible, needs human approval"));
        assert!(p.user.contains("Forbidden: close"));
        assert!(p.user.contains("orders=3"));
        let c = prompt(&ctx(true));
        assert!(c.system.contains("exactly one word"));
    }

    #[test]
    fn replies_become_inferred_records_or_nothing() {
        let c = ctx(false);
        assert!(parse_reply(&c, "{\"choice\": \"nothing\"}").is_empty());
        let r = parse_reply(
            &c,
            "Sure. {\"choice\":\"action\",\"action\":\"discount\",\"reason\":\"slow day\"}",
        );
        assert!(
            matches!(&r[..], [Record::Proposal { claim: Claim::Inferred, action, reason, .. }] if action == "discount" && reason == "slow day")
        );
        let r = parse_reply(&c, "{\"choice\":\"action\",\"action\":\"close\"}");
        assert!(r.is_empty(), "forbidden and unpermitted names are dropped");
        let r = parse_reply(
            &c,
            "{\"choice\":\"ask\",\"question\":\"Refund order 7?\",\"action\":\"refund\"}",
        );
        assert!(
            matches!(&r[..], [Record::Ask { claim: Claim::Inferred, action: Some(a), question, .. }] if a == "refund" && question == "Refund order 7?")
        );
        assert!(parse_reply(&c, "I would rather not say.").is_empty());
        assert!(parse_reply(&c, "").is_empty());
    }

    #[test]
    fn payloads_come_through_in_the_levers_shape() {
        let c = ctx(false);
        let p = prompt(&c);
        assert!(
            p.user
                .contains("announce: at most 2/day, reversible, takes text"),
            "{}",
            p.user
        );
        assert!(p.system.contains("\"payload\""), "{}", p.system);
        let r = parse_reply(
            &c,
            "{\"choice\":\"action\",\"action\":\"announce\",\"reason\":\"news\",\"payload\":\"Hello all\"}",
        );
        assert!(
            matches!(&r[..], [Record::Proposal { action, payload: Some(Payload::Text { text }), .. }] if action == "announce" && text == "Hello all"),
            "{r:?}"
        );
        // The wrong shape, or a lever that takes none, loses the payload.
        let r = parse_reply(
            &c,
            "{\"choice\":\"action\",\"action\":\"announce\",\"payload\":{\"x\":1}}",
        );
        assert!(
            matches!(&r[..], [Record::Proposal { payload: None, .. }]),
            "{r:?}"
        );
        let r = parse_reply(
            &c,
            "{\"choice\":\"action\",\"action\":\"discount\",\"payload\":\"ten percent\"}",
        );
        assert!(
            matches!(&r[..], [Record::Proposal { payload: None, .. }]),
            "{r:?}"
        );
        let r = parse_reply(
            &c,
            "{\"choice\":\"ask\",\"action\":\"announce\",\"question\":\"send this?\",\"payload\":\"Hi\"}",
        );
        assert!(
            matches!(&r[..], [Record::Ask { payload: Some(Payload::Text { text }), .. }] if text == "Hi"),
            "{r:?}"
        );
    }

    #[test]
    fn labels_work_for_classifiers() {
        let c = ctx(true);
        assert!(parse_reply(&c, "nothing\n").is_empty());
        assert!(parse_reply(&c, "Nothing.").is_empty());
        let r = parse_reply(&c, "discount");
        assert!(
            matches!(&r[..], [Record::Proposal { action, reason, .. }] if action == "discount" && reason == "chosen by test")
        );
        let r = parse_reply(&c, "action: refund -- customer is angry");
        assert!(
            matches!(&r[..], [Record::Proposal { action, reason, .. }] if action == "refund" && reason == "customer is angry")
        );
        let r = parse_reply(&c, "ask: should we extend the sale?");
        assert!(
            matches!(&r[..], [Record::Ask { action: None, question, .. }] if question == "should we extend the sale?")
        );
        let r = parse_reply(&c, "ask");
        assert!(matches!(&r[..], [Record::Ask { .. }]));
        assert!(parse_reply(&c, "launch").is_empty());
    }

    #[test]
    fn budget_keeps_the_newest() {
        let mut entries = vec![];
        let mut prev = GENESIS.to_string();
        for i in 1..=50u64 {
            let e = Entry::new(
                i,
                i as i64,
                &prev,
                Record::report(Claim::Measured, "shop", "orders", Some(i as f64), ""),
            );
            prev = e.hash.clone();
            entries.push(e);
        }
        let kept = trim_to_budget(&entries, 50);
        assert!(!kept.is_empty() && kept.len() < 50);
        assert_eq!(kept.last().unwrap().seq, 50);
        assert!(kept.windows(2).all(|w| w[0].seq < w[1].seq));
        assert_eq!(
            trim_to_budget(&entries, 1).len(),
            1,
            "at least one entry is kept"
        );
    }
}

//! The planner's search: pure, deterministic under a seed, no I/O.
//!
//! Given ensemble members (a world model and a seed each), a state, the
//! candidate levers and the guards, it enumerates sequences of up to
//! `depth` levers, one a step from now, runs each through every member to
//! the horizon, and ranks them by the worst member's target in the better
//! direction. Any member breaching a guard vetoes the sequence. The choice
//! is the first lever of the best sequence; `nothing` is always weighed.
//!
//! Exploration is Thompson sampling among `nothing` and the levers listed
//! to explore that are reversible, can run without a decision, and have no
//! member breaching a guard: one member drawn at random per candidate, the
//! argmax taken. It applies only when the pessimistic best is itself one of
//! them. Propensities are the draw frequencies over `DRAWS` seeded draws; a
//! pessimistic choice has propensity 1 and the rest 0.
//!
//! Everything here is inferred by construction: it ranks, vetoes and asks,
//! and never authorizes.

use serde_json::Value;

use crate::mandate::{Guard, Worse};
use crate::record::{Alternative, Quantiles, MAX_ACTION_NAME, MAX_ALTERNATIVES};
use crate::world::{self, Action, WorldModel};

/// What the planner calls doing no lever.
pub const NOTHING: &str = "nothing";
/// How many seeded draws estimate the propensities.
pub const DRAWS: u64 = 256;

/// A lever the planner may weigh.
#[derive(Clone, Debug, PartialEq)]
pub struct Lever {
    pub name: String,
    pub reversible: bool,
    /// Listed in the controller's `explore:`.
    pub explore: bool,
    /// Suspended or otherwise needing a decision: weighed, but it can only
    /// become an ask, so it is never explored.
    pub asks: bool,
    /// Runs left under its cap: how often a sequence may pull it.
    pub left: u32,
}

/// One ensemble member: a world model and its seed.
pub struct Member<'a> {
    pub model: &'a dyn WorldModel,
    pub seed: u64,
}

/// `n` members of one model, seeded from `base`.
pub fn members(model: &dyn WorldModel, base: u64, n: u32) -> Vec<Member<'_>> {
    (0..u64::from(n))
        .map(|i| Member {
            model,
            seed: world::member_seed(base, i),
        })
        .collect()
}

/// The seed of step `step` of a member's rollout.
pub fn step_seed(member: u64, step: u32) -> u64 {
    world::mix(member, u64::from(step))
}

/// What a search is asked.
pub struct Problem<'a> {
    pub state: Value,
    pub now: i64,
    /// Seconds a step: the venture's cadence.
    pub dt: i64,
    pub horizon: u32,
    pub depth: u32,
    pub better: Worse,
    pub guards: &'a [Guard],
    pub levers: Vec<Lever>,
    /// What the choice among explored levers draws from.
    pub seed: u64,
}

/// A sequence run through every member.
#[derive(Clone, Debug, PartialEq)]
pub struct Weighed {
    /// Lever names, one a step from now; empty for nothing.
    pub sequence: Vec<String>,
    /// The target at the horizon, one per member, in member order.
    pub targets: Vec<f64>,
    /// The same, signed so that higher is better.
    pub scores: Vec<f64>,
}

impl Weighed {
    /// The first action: a lever, or nothing.
    pub fn action(&self) -> &str {
        self.sequence.first().map_or(NOTHING, String::as_str)
    }

    /// The worst member's score: what the planner ranks by.
    pub fn worst(&self) -> f64 {
        worst(&self.scores)
    }

    /// The mean score: what the planner does not rank by.
    pub fn mean(&self) -> f64 {
        if self.scores.is_empty() {
            return f64::NEG_INFINITY;
        }
        self.scores.iter().sum::<f64>() / self.scores.len() as f64
    }

    /// The worst member's target, in the metric's own terms.
    pub fn worst_target(&self, better: Worse) -> f64 {
        match better {
            Worse::Up => self.worst(),
            Worse::Down => -self.worst(),
        }
    }

    pub fn quantiles(&self) -> Quantiles {
        world::quantiles(&self.targets).unwrap_or(Quantiles::from_values([f64::NAN; 7]))
    }
}

/// Pessimism: the worst member counts.
pub fn worst(scores: &[f64]) -> f64 {
    scores.iter().copied().fold(f64::INFINITY, f64::min)
}

/// What the search found.
#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    /// The best sequence of each first action, best first; vetoed
    /// sequences are not here.
    pub ranked: Vec<Weighed>,
    /// Sequences a member's guard reading vetoed, with the guard.
    pub vetoed: Vec<(Vec<String>, String)>,
    /// Index into `ranked` of the choice.
    pub chosen: usize,
    /// Did Thompson sampling choose?
    pub explored: bool,
    /// The chance each of `ranked` had of being chosen.
    pub propensities: Vec<f64>,
    /// Does the best stand apart from the runner-up?
    pub separated: bool,
}

impl Plan {
    pub fn choice(&self) -> &Weighed {
        &self.ranked[self.chosen]
    }

    /// The chosen and the top rejected alternatives, as a branch records
    /// them: target quantiles, the worst member's target, propensity.
    pub fn alternatives(&self, better: Worse) -> (Alternative, Vec<Alternative>) {
        let alt = |i: usize| {
            let w = &self.ranked[i];
            Alternative {
                action: w.action().chars().take(MAX_ACTION_NAME).collect(),
                quantiles: w.quantiles(),
                worst: w.worst_target(better),
                propensity: self.propensities[i],
            }
        };
        let rest = (0..self.ranked.len())
            .filter(|i| *i != self.chosen)
            .take(MAX_ALTERNATIVES)
            .map(alt)
            .collect();
        (alt(self.chosen), rest)
    }

    /// The first actions of the top three, best first: what a backend
    /// chooses among when branches cannot be separated.
    pub fn top(&self) -> Vec<&str> {
        self.ranked.iter().take(3).map(Weighed::action).collect()
    }
}

/// Every sequence of up to `depth` levers, one a step, within the horizon,
/// no lever more often than it has runs left. Nothing comes first.
pub fn sequences(levers: &[Lever], depth: u32, horizon: u32) -> Vec<Vec<usize>> {
    let depth = depth.min(horizon) as usize;
    let mut out = vec![vec![]];
    let mut frontier: Vec<Vec<usize>> = vec![vec![]];
    for _ in 0..depth {
        let mut next = Vec::new();
        for seq in &frontier {
            for (i, l) in levers.iter().enumerate() {
                let used = seq.iter().filter(|j| **j == i).count() as u32;
                if used < l.left {
                    let mut s = seq.clone();
                    s.push(i);
                    next.push(s);
                }
            }
        }
        out.extend(next.iter().cloned());
        frontier = next;
    }
    out
}

/// Run one sequence through every member to the horizon. A member whose
/// guard reading breaches a guard at any step vetoes it: Err(guard).
pub fn rollout(
    members: &[Member],
    p: &Problem,
    sequence: &[String],
) -> Result<Result<Weighed, String>, String> {
    let mut targets = Vec::with_capacity(members.len());
    for m in members {
        let mut state = p.state.clone();
        let mut metric = None;
        for step in 0..p.horizon {
            let action = sequence.get(step as usize).map(|name| Action {
                name: name.clone(),
                payload: None,
            });
            let next = m.model.step(
                &state,
                action.as_ref(),
                step_seed(m.seed, step),
                p.now + i64::from(step) * p.dt,
                p.dt,
            )?;
            if let Some(g) = p
                .guards
                .iter()
                .find(|g| next.guards.get(&g.name).is_some_and(|r| g.breached(*r)))
            {
                return Ok(Err(g.name.clone()));
            }
            metric = Some(next.metric);
            state = next.state;
        }
        let Some(x) = metric else {
            return Err("a horizon of no steps reaches nothing".to_string());
        };
        targets.push(x);
    }
    let scores = targets
        .iter()
        .map(|x| match p.better {
            Worse::Up => *x,
            Worse::Down => -*x,
        })
        .collect();
    Ok(Ok(Weighed {
        sequence: sequence.to_vec(),
        targets,
        scores,
    }))
}

/// Can the best be told apart from the runner-up? Not when the best's
/// worst member lies within the runner-up's p25 to p75.
pub fn separated(best: &Weighed, runner: &Weighed) -> bool {
    let Some(q) = world::quantiles(&runner.scores) else {
        return true;
    };
    let w = best.worst();
    !(q.p25 <= w && w <= q.p75)
}

/// One Thompson draw among `eligible` (indices into `ranked`): a member at
/// random for each, the argmax taken, the earlier on a tie.
pub fn thompson(ranked: &[Weighed], eligible: &[usize], seed: u64) -> usize {
    let mut rng = seed;
    let mut best: Option<(usize, f64)> = None;
    for &i in eligible {
        let scores = &ranked[i].scores;
        let m = (world::splitmix64(&mut rng) % scores.len().max(1) as u64) as usize;
        let x = scores.get(m).copied().unwrap_or(f64::NEG_INFINITY);
        if best.is_none_or(|(_, b)| x > b) {
            best = Some((i, x));
        }
    }
    best.map_or(eligible[0], |(i, _)| i)
}

/// The seed of draw `n` of a plan seeded `seed`.
pub fn draw_seed(seed: u64, n: u64) -> u64 {
    world::mix(seed, n)
}

/// The chance each of `ranked` has of a Thompson draw, over `DRAWS` draws.
pub fn propensities(ranked: &[Weighed], eligible: &[usize], seed: u64) -> Vec<f64> {
    let mut counts = vec![0u64; ranked.len()];
    for n in 0..DRAWS {
        counts[thompson(ranked, eligible, draw_seed(seed, n))] += 1;
    }
    counts
        .into_iter()
        .map(|c| c as f64 / DRAWS as f64)
        .collect()
}

/// Which of `ranked` may be explored: nothing, and the levers listed to
/// explore that are reversible and need no decision. Vetoed sequences are
/// not ranked at all.
pub fn explorable(ranked: &[Weighed], levers: &[Lever]) -> Vec<usize> {
    ranked
        .iter()
        .enumerate()
        .filter(|(_, w)| match w.sequence.first() {
            None => true,
            Some(name) => levers
                .iter()
                .find(|l| &l.name == name)
                .is_some_and(|l| l.explore && l.reversible && !l.asks),
        })
        .map(|(i, _)| i)
        .collect()
}

/// Search every sequence and choose.
pub fn search(members: &[Member], p: &Problem) -> Result<Plan, String> {
    if members.is_empty() {
        return Err("a plan needs at least one ensemble member".to_string());
    }
    if p.horizon == 0 {
        return Err("a plan needs a horizon of at least one step".to_string());
    }
    let mut best_of: Vec<Weighed> = Vec::new();
    let mut vetoed = Vec::new();
    for seq in sequences(&p.levers, p.depth, p.horizon) {
        let names: Vec<String> = seq.iter().map(|i| p.levers[*i].name.clone()).collect();
        match rollout(members, p, &names)? {
            Err(guard) => vetoed.push((names, guard)),
            Ok(w) => match best_of.iter_mut().find(|b| b.action() == w.action()) {
                Some(b) if better(&w, b) => *b = w,
                Some(_) => {}
                None => best_of.push(w),
            },
        }
    }
    if best_of.is_empty() {
        return Err(match vetoed.first() {
            Some((_, g)) => format!("every sequence, nothing included, breaches guard {g}"),
            None => "no sequence to weigh".to_string(),
        });
    }
    // Stable: on a full tie the earlier weighed, nothing first, stays ahead.
    best_of.sort_by(|a, b| {
        b.worst()
            .total_cmp(&a.worst())
            .then(b.mean().total_cmp(&a.mean()))
    });
    let ranked = best_of;
    let separated = ranked.get(1).is_none_or(|r| separated(&ranked[0], r));
    let eligible = explorable(&ranked, &p.levers);
    let explore = eligible.len() >= 2 && eligible.contains(&0);
    let (chosen, propensities) = if explore {
        (
            thompson(&ranked, &eligible, draw_seed(p.seed, u64::MAX)),
            propensities(&ranked, &eligible, p.seed),
        )
    } else {
        let mut ps = vec![0.0; ranked.len()];
        ps[0] = 1.0;
        (0, ps)
    };
    Ok(Plan {
        ranked,
        vetoed,
        chosen,
        explored: explore,
        propensities,
        separated,
    })
}

/// Is `a` better than `b`: a higher worst member, then a higher mean.
fn better(a: &Weighed, b: &Weighed) -> bool {
    a.worst()
        .total_cmp(&b.worst())
        .then(a.mean().total_cmp(&b.mean()))
        .is_gt()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mandate::Then;
    use crate::world::Next;
    use std::collections::{BTreeMap, BTreeSet};

    /// A toy world: the state is the target; each lever adds its mean plus
    /// uniform noise of its spread, the same for the same seed. A lever in
    /// `guarded` also reads the `spend` guard: 0, or 100 on a seed in `bad`.
    struct Toy {
        effects: BTreeMap<&'static str, (f64, f64)>,
        guarded: BTreeSet<&'static str>,
        bad: BTreeSet<u64>,
    }

    fn toy(effects: &[(&'static str, f64, f64)]) -> Toy {
        Toy {
            effects: effects.iter().map(|(n, m, s)| (*n, (*m, *s))).collect(),
            guarded: BTreeSet::new(),
            bad: BTreeSet::new(),
        }
    }

    impl WorldModel for Toy {
        fn name(&self) -> &'static str {
            "world"
        }

        fn step(
            &self,
            state: &Value,
            action: Option<&Action>,
            seed: u64,
            _now: i64,
            _dt: i64,
        ) -> Result<Next, String> {
            let mut x = state.as_f64().unwrap_or(0.0);
            let mut guards = BTreeMap::new();
            if let Some(a) = action {
                let (mean, spread) = self.effects[a.name.as_str()];
                let mut rng = seed;
                let u = (world::splitmix64(&mut rng) >> 11) as f64 / (1u64 << 53) as f64;
                x += mean + spread * (2.0 * u - 1.0);
                if self.guarded.contains(a.name.as_str()) {
                    let reading = if self.bad.contains(&seed) { 100.0 } else { 0.0 };
                    guards.insert("spend".to_string(), reading);
                }
            }
            Ok(Next {
                state: Value::from(x),
                metric: x,
                guards,
            })
        }
    }

    fn lever(name: &str) -> Lever {
        Lever {
            name: name.to_string(),
            reversible: true,
            explore: false,
            asks: false,
            left: 9,
        }
    }

    fn problem<'a>(levers: Vec<Lever>, guards: &'a [Guard]) -> Problem<'a> {
        Problem {
            state: Value::from(0.0),
            now: 1_800_000_000,
            dt: 3600,
            horizon: 1,
            depth: 1,
            better: Worse::Up,
            guards,
            levers,
            seed: 11,
        }
    }

    fn spend(above: f64) -> Guard {
        Guard {
            name: "spend".to_string(),
            run: Some("./spend".to_string()),
            adapter: None,
            above: Some(above),
            below: None,
            then: Then::Pause,
            drawdown: None,
            of: None,
        }
    }

    fn of<'a>(plan: &'a Plan, action: &str) -> &'a Weighed {
        plan.ranked.iter().find(|w| w.action() == action).unwrap()
    }

    #[test]
    fn the_worst_member_counts_not_the_average() {
        // A raises the target by 5 +/- 1, B by 8 +/- 10.
        let world = toy(&[("a", 5.0, 1.0), ("b", 8.0, 10.0)]);
        let ms = members(&world, 42, 16);
        let p = problem(vec![lever("a"), lever("b")], &[]);
        let plan = search(&ms, &p).unwrap();
        let (a, b) = (of(&plan, "a"), of(&plan, "b"));
        println!(
            "a: worst {:.2} mean {:.2}; b: worst {:.2} mean {:.2}",
            a.worst(),
            a.mean(),
            b.worst(),
            b.mean()
        );
        // Pessimistic: A, whose worst member beats B's.
        assert_eq!(plan.choice().action(), "a");
        assert!(a.worst() > 4.0 && b.worst() < 4.0, "{a:?} {b:?}");
        // An average-based choice would have picked B.
        assert!(
            b.mean() > a.mean(),
            "a mean {} b mean {}",
            a.mean(),
            b.mean()
        );
        // Pessimistic choices carry propensity 1 for the chosen, 0 else.
        assert!(!plan.explored);
        assert_eq!(plan.propensities[plan.chosen], 1.0);
        assert_eq!(plan.propensities.iter().sum::<f64>(), 1.0);
    }

    #[test]
    fn the_better_direction_flips_the_ranking() {
        let world = toy(&[("up", 5.0, 0.5), ("down", -5.0, 0.5)]);
        let ms = members(&world, 1, 8);
        let mut p = problem(vec![lever("up"), lever("down")], &[]);
        assert_eq!(search(&ms, &p).unwrap().choice().action(), "up");
        p.better = Worse::Down;
        let plan = search(&ms, &p).unwrap();
        assert_eq!(plan.choice().action(), "down");
        // Recorded in the metric's own terms: its worst is the highest.
        let (chosen, _) = plan.alternatives(Worse::Down);
        assert!(chosen.worst > -5.5 && chosen.worst < -4.5, "{chosen:?}");
        assert!(chosen.worst >= chosen.quantiles.p95 - 1e-9, "{chosen:?}");
    }

    #[test]
    fn any_member_breaching_a_guard_vetoes_the_sequence() {
        // B would win by far, and reads the spend guard.
        let mut world = toy(&[("a", 5.0, 1.0), ("b", 20.0, 1.0)]);
        world.guarded.insert("b");
        let guards = [spend(50.0)];
        let p = problem(vec![lever("a"), lever("b")], &guards);
        assert_eq!(
            search(&members(&world, 42, 16), &p)
                .unwrap()
                .choice()
                .action(),
            "b"
        );
        // One member of sixteen breaches it on B's first step.
        world.bad.insert(step_seed(world::member_seed(42, 7), 0));
        let plan = search(&members(&world, 42, 16), &p).unwrap();
        assert_eq!(plan.choice().action(), "a");
        assert!(plan.ranked.iter().all(|w| w.action() != "b"));
        assert_eq!(
            plan.vetoed,
            vec![(vec!["b".to_string()], "spend".to_string())]
        );
        // Deeper, every sequence with B in it is vetoed too, a breach on a
        // later step as much as on the first.
        world.bad.insert(step_seed(world::member_seed(42, 7), 1));
        let deep = Problem {
            depth: 2,
            horizon: 2,
            ..problem(vec![lever("a"), lever("b")], &guards)
        };
        let plan = search(&members(&world, 42, 16), &deep).unwrap();
        assert!(plan
            .ranked
            .iter()
            .all(|w| !w.sequence.contains(&"b".to_string())));
        assert!(plan.vetoed.iter().any(|(s, _)| s == &["a", "b"]));
    }

    #[test]
    fn sequences_respect_depth_horizon_and_caps() {
        let mut a = lever("a");
        a.left = 1;
        let levers = vec![a, lever("b")];
        let seqs = sequences(&levers, 2, 4);
        // nothing; a; b; then a,b  b,a  b,b (a,a is over its cap).
        assert_eq!(
            seqs,
            vec![vec![], vec![0], vec![1], vec![0, 1], vec![1, 0], vec![1, 1]]
        );
        assert_eq!(sequences(&levers, 3, 1).len(), 3);
    }

    #[test]
    fn branches_within_the_runner_ups_spread_are_not_separated() {
        // B's worst, about 4.9, lies within A's p25 to p75, about 4.6 to
        // 5.1: B ranks first, but cannot be told apart from A.
        let world = toy(&[("a", 5.0, 1.0), ("b", 5.0, 0.1)]);
        let ms = members(&world, 3, 16);
        let plan = search(&ms, &problem(vec![lever("a"), lever("b")], &[])).unwrap();
        assert_eq!(plan.choice().action(), "b");
        assert!(!plan.separated, "{plan:?}");
        assert_eq!(plan.top(), vec!["b", "a", NOTHING]);
        // B's worst, about 6.9, is past A's p75.
        let world = toy(&[("a", 5.0, 1.0), ("b", 7.0, 0.1)]);
        let ms = members(&world, 3, 16);
        let plan = search(&ms, &problem(vec![lever("a"), lever("b")], &[])).unwrap();
        assert!(plan.separated, "{plan:?}");
    }

    /// Two explorable levers close enough that Thompson splits between
    /// them, and one not listed.
    fn explore_case() -> (Toy, Vec<Lever>) {
        let world = toy(&[("a", 5.0, 3.0), ("b", 5.5, 3.0), ("c", 1.0, 0.1)]);
        let mut a = lever("a");
        a.explore = true;
        let mut b = lever("b");
        b.explore = true;
        (world, vec![a, b, lever("c")])
    }

    #[test]
    fn explore_propensities_sum_to_one_and_match_the_draws() {
        let (world, levers) = explore_case();
        let ms = members(&world, 5, 16);
        let p = problem(levers, &[]);
        let plan = search(&ms, &p).unwrap();
        assert!(plan.explored);
        let total: f64 = plan.propensities.iter().sum();
        assert!((total - 1.0).abs() < 1e-12, "{total}");
        // Independent draws, far more of them, from other seeds.
        let eligible = explorable(&plan.ranked, &p.levers);
        let n = 20_000u64;
        let mut freq = vec![0.0; plan.ranked.len()];
        for k in 0..n {
            freq[thompson(&plan.ranked, &eligible, world::mix(999, k))] += 1.0 / n as f64;
        }
        for (i, (p, f)) in plan.propensities.iter().zip(&freq).enumerate() {
            assert!(
                (p - f).abs() < 0.08,
                "{}: propensity {p} against frequency {f}",
                plan.ranked[i].action()
            );
        }
        // Both explored levers have a real chance; the unlisted one none.
        assert!(plan.propensities[plan.chosen] > 0.0);
        assert!(
            plan.propensities[plan.ranked.iter().position(|w| w.action() == "a").unwrap()] > 0.1
        );
        assert!(
            plan.propensities[plan.ranked.iter().position(|w| w.action() == "b").unwrap()] > 0.1
        );
        assert_eq!(
            plan.propensities[plan.ranked.iter().position(|w| w.action() == "c").unwrap()],
            0.0
        );
    }

    #[test]
    fn explore_never_picks_an_irreversible_or_guard_breaching_lever() {
        // a and d are explorable and close; b is listed to explore, as
        // close, but irreversible.
        let world = toy(&[("a", 5.5, 3.0), ("b", 5.2, 3.0), ("d", 5.0, 3.0)]);
        let mut levers = vec![lever("a"), lever("b"), lever("d")];
        for l in &mut levers {
            l.explore = true;
        }
        levers[1].reversible = false;
        let ms = members(&world, 5, 16);
        let plan = search(&ms, &problem(levers.clone(), &[])).unwrap();
        assert!(plan.explored, "{plan:?}");
        let at = |plan: &Plan, a: &str| plan.ranked.iter().position(|w| w.action() == a).unwrap();
        assert_eq!(plan.propensities[at(&plan, "b")], 0.0, "{plan:?}");
        let mut chosen = BTreeSet::new();
        for seed in 0..200 {
            let p = Problem {
                seed,
                ..problem(levers.clone(), &[])
            };
            let plan = search(&ms, &p).unwrap();
            assert!(plan.explored);
            chosen.insert(plan.choice().action().to_string());
        }
        assert_eq!(
            chosen,
            BTreeSet::from(["a".to_string(), "d".to_string()]),
            "explore picks among a and d only"
        );
        // A lever that needs a decision is not explored either.
        let mut asking = levers.clone();
        asking[1].reversible = true;
        asking[1].asks = true;
        let plan = search(&ms, &problem(asking, &[])).unwrap();
        assert!(plan.explored);
        assert_eq!(plan.propensities[at(&plan, "b")], 0.0, "{plan:?}");
        // Nor one a member's guard reading breaches: it is not ranked.
        let (mut world, levers) = explore_case();
        let levers = levers[..2].to_vec();
        world.guarded.insert("b");
        let guards = [spend(50.0)];
        let ms = members(&world, 5, 16);
        let p = |seed| Problem {
            seed,
            ..problem(levers.clone(), &guards)
        };
        // Without a breach, explore splits between a and b.
        let split: BTreeSet<String> = (0..50)
            .map(|seed| search(&ms, &p(seed)).unwrap().choice().action().to_string())
            .collect();
        assert!(split.contains("b"), "{split:?}");
        // One member breaching on b: b is never picked.
        world.bad.insert(step_seed(world::member_seed(5, 2), 0));
        let ms = members(&world, 5, 16);
        for seed in 0..50 {
            let plan = search(&ms, &p(seed)).unwrap();
            assert_eq!(plan.choice().action(), "a", "seed {seed}");
            assert!(plan.ranked.iter().all(|w| w.action() != "b"));
        }
    }

    #[test]
    fn the_same_seed_gives_the_same_plan() {
        let (world, levers) = explore_case();
        let run = |base: u64, seed: u64| {
            let ms = members(&world, base, 16);
            search(
                &ms,
                &Problem {
                    seed,
                    ..problem(levers.clone(), &[])
                },
            )
            .unwrap()
        };
        assert_eq!(run(5, 1), run(5, 1));
        assert_ne!(run(5, 1).ranked, run(6, 1).ranked);
    }
}

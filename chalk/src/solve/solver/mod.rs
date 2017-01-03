#![allow(unused_variables)]

use infer::*;
use formula::*;
use solve::*;
use subst::Subst;
use std::collections::VecDeque;
use std::sync::Arc;

mod instantiate;
use self::instantiate::InstantiateError;

pub struct Solver {
    infer: InferenceTable,
    root_goal: Goal<Application>,
    obligations: VecDeque<Obligation>,
    choice_points: Vec<ChoicePoint>,
    strategy: Strategy,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Strategy {
    // Prolog-style search
    DepthFirstSearch,

    // Rust-style search, proceed only when unambiguous
    Rust,

    // "Input output" style, in which the clause implications guide
    // us in deciding what to do.
    InOut,
}

struct ChoicePoint {
    obligations: VecDeque<Obligation>,
    infer_snapshot: InferenceSnapshot,
    kind: ChoicePointKind,
}

enum ChoicePointKind {
    Clauses(ChoicePointClauses),
    Disjunction(ChoicePointDisjunction),
}

struct ChoicePointClauses {
    clauses: VecDeque<Clause<Application>>,
    environment: Arc<Environment>,
    application: Application,
    depth: usize,
}

struct ChoicePointDisjunction {
    goals: VecDeque<Obligation>,
}

enum ProveError {
    NotProvable,
    Overflow,
    Ambiguous,
}

// Indicates that there are no more choicepoints
struct UnrollError;

impl Solver {
    pub fn new(root_environment: &Arc<Environment>,
               root_goal: &Goal<Application>,
               strategy: Strategy)
               -> Self {
        let mut infer = InferenceTable::new();
        let root_goal = infer.peel_goal(root_environment, root_goal);
        Solver {
            infer: infer,
            root_goal: root_goal.clone(),
            obligations: vec![Obligation::new(root_environment.clone(), root_goal, 0)].into(),
            choice_points: vec![],
            strategy: strategy,
        }
    }

    fn fork(&self, goal: &Goal<Application>) -> Self {
        Solver {
            infer: self.infer.clone(),
            root_goal: goal.clone(),
            obligations: VecDeque::new(),
            choice_points: vec![],
            strategy: self.strategy,
        }
    }

    fn canonicalize(&mut self, goal: &Goal<Application>) -> Goal<Application> {
        // FIXME -- this meant to replace unbound variables like ?F
        // with a `_`, but that is not a variant of leaf (and should
        // not be). Should be able to fix this without extending leaf
        // but might need to generalize folder trait.
        self.infer.normalize_deep(goal)
    }

    fn find_next_solution(&mut self) -> Result<String, ProveError> {
        debug!("find_next_solution: {} obligations", self.obligations.len());
        let mut stalled_obligations = VecDeque::new();
        loop {
            let mut progress = false;
            while let Some(obligation) = self.obligations.pop_back() {
                match self.solve_obligation(obligation)? {
                    Some(stalled_obligation) => {
                        debug!("find_next_solution: stalled");
                        stalled_obligations.push_back(stalled_obligation)
                    }
                    None => {
                        debug!("find_next_solution: progress");
                        progress = true;
                    }
                }
            }

            if stalled_obligations.is_empty() {
                break;
            }

            if !progress {
                debug!("find_next_solution: ambiguous result {:?}", {
                    for obligation in &self.obligations.clone() {
                        debug!("find_next_solution: {:?} stalled",
                               self.canonicalize(&obligation.goal));
                    }
                });
                return Err(ProveError::Ambiguous);
            }

            // the DFS strategy has no concept of a stalled obligation
            assert!(self.strategy != Strategy::DepthFirstSearch);

            while let Some(o) = stalled_obligations.pop_back() {
                self.obligations.push_front(o);
            }
            assert!(stalled_obligations.is_empty());
        }

        let goal = self.root_goal.clone();
        let goal = self.canonicalize(&goal);
        Ok(format!("{:?}", goal))
    }

    fn unroll(&mut self) -> Result<(), UnrollError> {
        if let Some(top_choice_point) = self.choice_points.pop() {
            let ChoicePoint { obligations, infer_snapshot, kind } = top_choice_point;
            self.obligations = obligations;
            self.infer.rollback_to(infer_snapshot);
            match kind {
                ChoicePointKind::Clauses(clauses) => self.start_next_clause(clauses),
                ChoicePointKind::Disjunction(disjunction) => {
                    self.start_next_disjunction(disjunction)
                }
            }
        } else {
            Err(UnrollError)
        }
    }

    fn start_next_clause(&mut self, clauses: ChoicePointClauses) -> Result<(), UnrollError> {
        let ChoicePointClauses { mut clauses, application, environment, depth } = clauses;

        'next_clause: while let Some(clause) = clauses.pop_front() {
            let snapshot = self.infer.snapshot();

            let ClauseImplication { condition, consequence } = self.infer
                .instantiate_existential(&environment, &clause);

            assert_eq!(application.constant_and_arity(),
                       consequence.constant_and_arity());
            for (leaf1, leaf2) in application.args.iter().zip(&consequence.args) {
                if let Err(e) = self.infer.unify(leaf1, leaf2) {
                    self.infer.rollback_to(snapshot);
                    continue 'next_clause;
                }
            }

            self.choice_points.push(ChoicePoint {
                obligations: self.obligations.clone(),
                infer_snapshot: snapshot,
                kind: ChoicePointKind::Clauses(ChoicePointClauses {
                    environment: environment.clone(),
                    clauses: clauses,
                    application: application,
                    depth: depth,
                }),
            });

            if let Some(goal) = condition {
                self.obligations.push_back(Obligation {
                    environment: environment.clone(),
                    goal: goal,
                    depth: depth,
                });
            }

            return Ok(());
        }

        self.unroll()
    }

    fn start_next_disjunction(&mut self,
                              disjunction: ChoicePointDisjunction)
                              -> Result<(), UnrollError> {
        let ChoicePointDisjunction { mut goals } = disjunction;

        while let Some(goal) = goals.pop_front() {
            let snapshot = self.infer.snapshot();

            self.choice_points.push(ChoicePoint {
                obligations: self.obligations.clone(),
                infer_snapshot: snapshot,
                kind: ChoicePointKind::Disjunction(ChoicePointDisjunction { goals: goals }),
            });

            self.obligations.push_back(goal);

            return Ok(());
        }

        self.unroll()
    }

    /// Returns:
    /// - Ok(Some(obligation)) => stalled, come back to obligation later
    /// - Ok(None) => solved, pushed more work on self.obligations
    /// - Err(_) => cannot solve
    fn solve_obligation(&mut self,
                        obligation: Obligation)
                        -> Result<Option<Obligation>, ProveError> {
        debug!("solve_obligation: goal={:?}",
               self.canonicalize(&obligation.goal));
        debug!("solve_obligation: depth={:?}", obligation.depth);
        let Obligation { environment, goal, depth } = obligation;
        if depth > 10 {
            return Err(ProveError::Overflow);
        }
        match goal.kind {
            GoalKind::True => Ok(None),
            GoalKind::False => Err(ProveError::NotProvable),
            GoalKind::Leaf(ref application) => {
                match self.strategy {
                    Strategy::Rust => self.solve_leaf_rust(&environment, &goal, application, depth),
                    Strategy::InOut => {
                        self.solve_leaf_in_out(&environment, &goal, application, depth)
                    }
                    Strategy::DepthFirstSearch => {
                        self.solve_leaf_dfs(environment, application, depth)
                    }
                }
            }
            GoalKind::IfThenElse(ref cond_goal, ref then_goal, ref else_goal) => {
                self.solve_if_then_else(&environment, &goal, cond_goal, then_goal, else_goal, depth)
            }
            GoalKind::And(ref g1, ref g2) => {
                // NB: Important that we consider g1 first
                self.obligations.extend([g2, g1]
                    .iter()
                    .map(|&goal| {
                        Obligation {
                            environment: environment.clone(),
                            goal: goal.clone(),
                            depth: depth,
                        }
                    }));
                Ok(None)
            }
            GoalKind::Or(ref g1, ref g2) => {
                match self.strategy {
                    Strategy::InOut | Strategy::Rust => {
                        // FIXME -- this is overly conservative. For
                        // example, if And(g1, g2) is provable, then
                        // Or(g1, g2) is clearly satisfied. Or, if g1
                        // or g2 contains no inference variables, and
                        // we can prove one of them, then again Or is
                        // satisfied. The danger is that we prove
                        // (say) g1 and then this influences inference
                        // in some way that proving g2 would not have
                        // done, and this affects later computation.
                        Err(ProveError::Ambiguous)
                    }
                    Strategy::DepthFirstSearch => {
                        let mut deque = VecDeque::new();
                        deque.push_back(Obligation {
                            environment: environment.clone(),
                            goal: g1.clone(),
                            depth: depth + 1,
                        });
                        deque.push_back(Obligation {
                            environment: environment.clone(),
                            goal: g2.clone(),
                            depth: depth + 1,
                        });
                        let disjunction = ChoicePointDisjunction { goals: deque };
                        match self.start_next_disjunction(disjunction) {
                            Ok(()) => Ok(None),
                            Err(UnrollError) => Err(ProveError::NotProvable),
                        }
                    }
                }
            }
            GoalKind::Exists(ref quant) => {
                let new_goal = self.infer.instantiate_existential(&environment, quant);
                self.obligations.push_back(Obligation {
                    environment: environment.clone(),
                    goal: new_goal,
                    depth: depth,
                });
                Ok(None)
            }
            GoalKind::ForAll(ref quant) => {
                assert!(quant.num_binders > 0);
                let mut new_environment = environment;
                let mut subst = None;
                for _ in 0..quant.num_binders {
                    new_environment = Arc::new(Environment::new(Some(new_environment), vec![]));
                    let depth = new_environment.depth();
                    subst = Some(Subst::new(subst.as_ref(), leaf!(apply (skol depth))));
                }
                let subst = subst.unwrap(); // always at least 1 binder
                let new_goal = subst.apply(quant.skip_binders());
                self.obligations.push_back(Obligation {
                    environment: new_environment,
                    goal: new_goal,
                    depth: depth,
                });
                Ok(None)
            }
            GoalKind::Implication(ref clauses, ref goal) => {
                let new_environment = Arc::new(Environment::new(Some(environment),
                                                                clauses.clone()));
                self.obligations.push_back(Obligation {
                    environment: new_environment,
                    goal: goal.clone(),
                    depth: depth,
                });
                Ok(None)
            }
        }
    }

    fn solve_leaf_in_out(&mut self,
                         environment: &Arc<Environment>,
                         goal: &Goal<Application>,
                         application: &Application,
                         depth: usize)
                         -> Result<Option<Obligation>, ProveError> {
        let choices: Result<Vec<_>, ()> = environment.clauses_relevant_to(application)
            .filter_map(|clause| {
                match self.infer.instantiate_clause(environment, application, clause) {
                    Ok(condition) => Some(Ok(condition)),
                    Err(InstantiateError::Ambiguous) => Some(Err(())),
                    Err(InstantiateError::Incompatible) => None,
                }
            })
            .collect();

        if choices.is_err() || choices.as_ref().unwrap().len() > 1 {
            debug!("encountered ambiguity: {:?}", choices);

            return Ok(Some(Obligation {
                environment: environment.clone(),
                goal: goal.clone(),
                depth: depth,
            }));
        }

        let mut choices = choices.unwrap();

        if choices.len() == 0 {
            return Err(ProveError::NotProvable);
        }

        if let Some(condition) = choices.pop().unwrap() {
            debug!("condition to be proven: {:?}", condition);
            let obligation = Obligation {
                environment: environment.clone(),
                goal: condition.clone(),
                depth: depth + 1,
            };
            self.obligations.push_back(obligation);
        }

        Ok(None)
    }

    fn solve_leaf_rust(&mut self,
                       environment: &Arc<Environment>,
                       goal: &Goal<Application>,
                       application: &Application,
                       depth: usize)
                       -> Result<Option<Obligation>, ProveError> {
        let mut choices: Vec<_> = environment.clauses_relevant_to(application)
            .filter(|clause| {
                let snapshot = self.infer.snapshot();
                let result = match self.unify_clause(environment, clause, application) {
                    Ok(_condition) => true,
                    Err(unify_error) => {
                        debug!("solve_leaf_rust: failed to unify because {:?}", unify_error);
                        false
                    }
                };
                self.infer.rollback_to(snapshot);
                result
            })
            .collect();

        debug!("found {} potential choices", choices.len());

        // try to winnow down our choices
        if choices.len() > 1 {
            self.winnow(environment, goal, application, depth, &mut choices);
            debug!("after winnowing, {} choices remain", choices.len());
        }

        if choices.len() == 0 {
            return Err(ProveError::NotProvable);
        }

        // put it at the back of the line
        if choices.len() > 1 {
            return Ok(Some(Obligation {
                environment: environment.clone(),
                goal: goal.clone(),
                depth: depth,
            }));
        }

        // if we have only one choice that succeeds, use it
        let clause = choices.pop().unwrap();
        self.push_clause_obligation(environment, clause, application, depth).unwrap();
        Ok(None)
    }

    fn push_clause_obligation(&mut self,
                              environment: &Arc<Environment>,
                              clause: &Clause<Application>,
                              application: &Application,
                              depth: usize)
                              -> UnifyResult<()> {
        let condition = self.unify_clause(environment, clause, application).unwrap();
        if let Some(condition) = condition {
            let obligation = Obligation {
                environment: environment.clone(),
                goal: condition.clone(),
                depth: depth + 1,
            };
            self.obligations.push_back(obligation);
        }
        Ok(())
    }

    fn winnow(&mut self,
              environment: &Arc<Environment>,
              goal: &Goal<Application>,
              application: &Application,
              depth: usize,
              choices: &mut Vec<&Clause<Application>>) {
        choices.retain(|clause| {
            let mut solver = self.fork(goal);
            solver.push_clause_obligation(environment, clause, application, depth).unwrap();
            let result = match solver.find_next_solution() {
                Ok(_) => true,
                Err(ProveError::NotProvable) => false,
                Err(ProveError::Ambiguous) => true,
                Err(ProveError::Overflow) => true,
            };
            debug!("winnow: clause={:?} result={:?}", clause, result);
            result
        });
    }

    fn solve_leaf_dfs(&mut self,
                      environment: Arc<Environment>,
                      application: &Application,
                      depth: usize)
                      -> Result<Option<Obligation>, ProveError> {
        let clauses: VecDeque<_> = environment.clauses_relevant_to(application).cloned().collect();
        let clauses = ChoicePointClauses {
            clauses: clauses,
            environment: environment,
            application: application.clone(),
            depth: depth + 1,
        };

        match self.start_next_clause(clauses) {
            Ok(()) => Ok(None),
            Err(UnrollError) => Err(ProveError::NotProvable),
        }
    }

    fn unify_clause(&mut self,
                    environment: &Environment,
                    clause: &Clause<Application>,
                    application: &Application)
                    -> UnifyResult<Option<Goal<Application>>> {
        let ClauseImplication { condition, consequence } = self.infer
            .instantiate_existential(&environment, clause);
        assert_eq!(application.constant_and_arity(),
                   consequence.constant_and_arity());
        for (leaf1, leaf2) in application.args.iter().zip(&consequence.args) {
            self.infer.unify(leaf1, leaf2)?;
        }
        Ok(condition)
    }

    fn solve_if_then_else(&mut self,
                          environment: &Arc<Environment>,
                          goal: &Goal<Application>, // if G1 then G2 else G3
                          cond_goal: &Goal<Application>, // G1
                          then_goal: &Goal<Application>, // G2
                          else_goal: &Goal<Application>, // G3
                          depth: usize)
                          -> Result<Option<Obligation>, ProveError> {
        match self.strategy {
            Strategy::Rust | Strategy::InOut => {
                self.solve_if_then_else_rust(environment,
                                             goal,
                                             cond_goal,
                                             then_goal,
                                             else_goal,
                                             depth)
            }
            Strategy::DepthFirstSearch => {
                self.solve_if_then_else_prolog(environment,
                                               goal,
                                               cond_goal,
                                               then_goal,
                                               else_goal,
                                               depth)
            }
        }
    }

    fn solve_if_then_else_prolog(&mut self,
                                 environment: &Arc<Environment>,
                                 goal: &Goal<Application>, // if G1 then G2 else G3
                                 cond_goal: &Goal<Application>, // G1
                                 then_goal: &Goal<Application>, // G2
                                 else_goal: &Goal<Application>, // G3
                                 depth: usize)
                                 -> Result<Option<Obligation>, ProveError> {
        // Meaning of `if/then/else` in prolog is dubious. Since
        // prolog strategy can't "defer" things, it just means
        // "provable at this time".

        self.solve_if_then_else_common(environment, goal, &cond_goal, then_goal, else_goal, depth)
    }

    fn solve_if_then_else_rust(&mut self,
                               environment: &Arc<Environment>,
                               goal: &Goal<Application>, // if G1 then G2 else G3
                               cond_goal: &Goal<Application>, // G1
                               then_goal: &Goal<Application>, // G2
                               else_goal: &Goal<Application>, // G3
                               depth: usize)
                               -> Result<Option<Obligation>, ProveError> {
        // we can only *reliably* test whether something is provable
        // if no inference is needed; actually, this predicate itself
        // is incomplete, since there could be inference variables in the
        // environment
        let cond_goal = self.canonicalize(&cond_goal);
        if ContainsInferenceVars::test(&cond_goal) {
            return Ok(Some(Obligation {
                environment: environment.clone(),
                goal: goal.clone(),
                depth: depth,
            }));
        }

        self.solve_if_then_else_common(environment, goal, &cond_goal, then_goal, else_goal, depth)
    }

    fn solve_if_then_else_common(&mut self,
                                 environment: &Arc<Environment>,
                                 goal: &Goal<Application>, // if G1 then G2 else G3
                                 cond_goal: &Goal<Application>, // G1
                                 then_goal: &Goal<Application>, // G2
                                 else_goal: &Goal<Application>, // G3
                                 depth: usize)
                                 -> Result<Option<Obligation>, ProveError> {
        // try to solve `cond`:
        let mut solver = self.fork(&cond_goal);
        solver.obligations
            .push_back(Obligation::new(environment.clone(), cond_goal.clone(), depth));
        match solver.find_next_solution() {
            Ok(_) => {
                self.obligations.push_back(Obligation {
                    environment: environment.clone(),
                    goal: then_goal.clone(),
                    depth: depth,
                });
                Ok(None)
            }
            Err(ProveError::NotProvable) => {
                self.obligations.push_back(Obligation {
                    environment: environment.clone(),
                    goal: else_goal.clone(),
                    depth: depth,
                });
                Ok(None)
            }
            Err(ProveError::Ambiguous) => Err(ProveError::Ambiguous),
            Err(ProveError::Overflow) => Err(ProveError::Overflow),
        }
    }
}

impl InferenceTable {
    fn instantiate_existential<F>(&mut self,
                                  environment: &Environment,
                                  quant: &Quantification<F>)
                                  -> F
        where F: Fold + Clone
    {
        let mut subst = None;
        for _ in 0..quant.num_binders {
            let var = self.new_variable(environment.universe_index()).to_leaf();
            subst = Some(Subst::new(subst.as_ref(), var));
        }
        subst.map(|subst| subst.apply(quant.skip_binders()))
            .unwrap_or(quant.skip_binders().clone())
    }

    fn peel_goal(&mut self,
                 root_environment: &Arc<Environment>,
                 goal: &Goal<Application>)
                 -> Goal<Application> {
        let mut goal = goal.clone();

        // If the goal is `(exists X -> ...)`, then we instantiate `X`
        // with an inference variable and set `...` as our new "root
        // goal". This way, when we find solutions, we will print out
        // the value of `X` that made it true, and not just `exists X
        // -> ...`.
        loop {
            match goal.clone().kind {
                GoalKind::Exists(ref quant) => {
                    let formula = self.instantiate_existential(root_environment, quant);
                    goal = formula;
                }
                _ => break,
            }
        }
        goal
    }
}

impl Iterator for Solver {
    type Item = String;

    fn next(&mut self) -> Option<String> {
        loop {
            if self.obligations.is_empty() {
                if self.choice_points.is_empty() {
                    return None;
                }

                match self.unroll() {
                    Ok(()) => { }
                    Err(UnrollError) => {
                        return None;
                    }
                }
            }

            match self.find_next_solution() {
                Ok(solution) => {
                    return Some(solution);
                }
                Err(ProveError::NotProvable) => {
                    self.obligations.clear();
                }
                Err(ProveError::Ambiguous) => {
                    self.obligations.clear();
                    return Some("<<ambiguous>>".to_string());
                }
                Err(ProveError::Overflow) => {
                    self.obligations.clear();
                    return Some("<<overflow>>".to_string());
                }
            }
        }
    }
}

#[cfg(test)]
mod test;
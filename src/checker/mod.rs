use std::rc::Rc;

use crate::parser::ast::*;

pub type Rule = fn(&[Rc<Term>], Vec<&ProofCommand>, &[ProofArg]) -> bool;

pub struct ProofChecker {
    proof: Proof,
}

impl ProofChecker {
    pub fn new(proof: Proof) -> Self {
        ProofChecker { proof }
    }

    pub fn check(self) -> bool {
        for step in &self.proof.0 {
            if let ProofCommand::Step {
                clause,
                rule,
                premises,
                args,
            } = step
            {
                let rule = Self::get_rule(rule);
                let premises = premises.iter().map(|&i| &self.proof.0[i]).collect();
                if !rule(&clause, premises, &args) {
                    return false;
                }
            }
        }
        true
    }

    fn get_rule(rule_name: &str) -> Rule {
        match rule_name {
            "or" => rules::or,
            "eq_congruent" => rules::eq_congruent,
            "resolution" => rules::resolution,
            _ => todo!(),
        }
    }
}

/// A macro to help deconstruct operation terms. Since a term holds references to other terms in
/// `Vec`s and `Rc`s, pattern matching a complex term can be difficult and verbose. This macro
/// helps with that.
macro_rules! match_op {
    ($bind:ident = $var:expr) => {
        Some($var)
    };
    (($op:tt $($args:tt)+) = $var:expr) => {{
        let _: &Term = $var;
        if let Term::Op(match_op!(@GET_VARIANT $op), args) = $var {
            match_op!(@ARGS ($($args)+) = args.as_slice())
        } else {
            None
        }
    }};
    (@ARGS ($arg:tt) = $var:expr) => {
        if let [arg] = $var {
            match_op!($arg = arg.as_ref())
        } else {
            None
        }
    };
    (@ARGS ($arg1:tt $arg2:tt) = $var:expr) => {
        if let [arg1, arg2] = $var {
            match (match_op!($arg1 = arg1.as_ref()), match_op!($arg2 = arg2.as_ref())) {
                (Some(arg1), Some(arg2)) => Some((arg1, arg2)),
                _ => None,
            }
        } else {
            None
        }
    };
    (@GET_VARIANT not) => { Operator::Not };
    (@GET_VARIANT =) => { Operator::Eq };
}

mod rules {
    use super::*;
    use std::collections::{hash_map::Entry, HashMap};

    pub fn or(clause: &[Rc<Term>], premises: Vec<&ProofCommand>, _: &[ProofArg]) -> bool {
        if premises.len() != 1 {
            return false;
        }
        let or_term = match premises[0] {
            ProofCommand::Assume(cl) => cl,
            ProofCommand::Step { clause, .. } => {
                if clause.len() == 1 {
                    &clause[0]
                } else {
                    return false;
                }
            }
        };
        let or_contents = if let Term::Op(Operator::Or, args) = or_term.as_ref() {
            args
        } else {
            return false;
        };

        or_contents == clause
    }

    pub fn eq_congruent(clause: &[Rc<Term>], _: Vec<&ProofCommand>, _: &[ProofArg]) -> bool {
        if clause.len() < 2 {
            return false;
        }

        // The first `clause.len()` - 1 terms in the clause must be a sequece of inequalites
        let mut ts = Vec::new();
        let mut us = Vec::new();
        for term in &clause[..clause.len() - 1] {
            if let Some((t, u)) = match_op!((not (= t u)) = term.as_ref()) {
                ts.push(t);
                us.push(u);
            } else {
                return false;
            }
        }

        // The final term in the clause must be an equality of two function applications, whose
        // arguments are the terms in the previous inequalities
        match match_op!((= f g) = clause.last().unwrap().as_ref()) {
            Some((Term::App(f, f_args), Term::App(g, g_args))) => {
                if f != g || f_args.len() != ts.len() {
                    return false;
                }
                for i in 0..ts.len() {
                    if f_args[i].as_ref() != ts[i] || g_args[i].as_ref() != us[i] {
                        return false;
                    }
                }
                true
            }
            _ => false,
        }
    }

    pub fn resolution(clause: &[Rc<Term>], premises: Vec<&ProofCommand>, _: &[ProofArg]) -> bool {
        /// Represents the polarities of a term encountered during checking.
        #[derive(Debug, PartialEq, Eq)]
        enum Polarity {
            Positive,
            Negative,
            Both,
        }

        /// Convert a term to positive polarity, and return its old polarity. Assumes that the term
        /// has at most one leading negation, that is, it is not of the form "(not (not ...))".
        fn to_positive(term: &Term) -> (&Term, Polarity) {
            match term {
                // We assume that the "not" term is well constructed, meaning it has exactly
                // one argument
                Term::Op(Operator::Not, args) => (args[0].as_ref(), Polarity::Negative),
                other => (other, Polarity::Positive),
            }
        }

        // For every term in each premise, we will convert it to positive polarity, and record
        // with which polarities it was encountered
        let mut encountered_polarities: HashMap<&Term, Polarity> = HashMap::new();
        for command in premises.into_iter() {
            let premise_clause = match command {
                // "assume" premises are interpreted as a clause with a single term
                ProofCommand::Assume(term) => std::slice::from_ref(term),
                ProofCommand::Step { clause, .. } => &clause,
            };
            for term in premise_clause {
                let (term, polarity) = to_positive(term.as_ref());
                match encountered_polarities.entry(term) {
                    // If the term is not in the hash map, we insert it
                    Entry::Vacant(entry) => {
                        entry.insert(polarity);
                    }

                    // If the term is in the hash map with the opposite polarity, we set the
                    // polarity to `Polarity::Both`
                    Entry::Occupied(mut entry) => {
                        if *entry.get() != Polarity::Both && *entry.get() != polarity {
                            entry.insert(Polarity::Both);
                        }
                    }
                }
            }
        }

        // We expect the final clause to be every term that appeared in the premises in only one
        // polarity, and we also expect these terms to be in the correct polarity
        let expected_len = encountered_polarities
            .iter()
            .filter(|&(_, polarity)| *polarity != Polarity::Both)
            .count();
        if clause.len() != expected_len {
            return false;
        }
        for t in clause {
            let (t, polarity) = to_positive(t.as_ref());
            if encountered_polarities.get(t) != Some(&polarity) {
                return false;
            }
        }

        true
    }
}

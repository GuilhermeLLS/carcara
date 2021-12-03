use super::{assert_clause_len, assert_eq, assert_num_args, RuleArgs, RuleResult};
use crate::{
    ast::*,
    checker::error::{CheckerError, LinearArithmeticError},
    utils::RawOps,
};
use ahash::AHashMap;
use num_bigint::BigInt;
use num_integer::Integer;
use num_rational::BigRational;
use num_traits::{One, Signed, Zero};

pub fn la_rw_eq(RuleArgs { conclusion, .. }: RuleArgs) -> RuleResult {
    assert_clause_len(conclusion, 1)?;

    let ((t_1, u_1), ((t_2, u_2), (u_3, t_3))) = match_term_err!(
        (= (= t u) (and (<= t u) (<= u t))) = &conclusion[0]
    )?;
    assert_eq(t_1, t_2)?;
    assert_eq(t_2, t_3)?;
    assert_eq(u_1, u_2)?;
    assert_eq(u_2, u_3)
}

/// Takes a disequality term and returns its negation, represented by an operator and two linear
/// combinations.
/// The disequality can be:
/// * An application of the "<", ">", "<=" or ">=" operators
/// * The negation of an application of one of these operators
/// * The negation of an application of the "=" operator
fn negate_disequality(term: &Rc<Term>) -> Result<(Operator, LinearComb, LinearComb), CheckerError> {
    use Operator::*;

    fn negate_operator(op: Operator) -> Option<Operator> {
        Some(match op {
            LessThan => GreaterEq,
            GreaterThan => LessEq,
            LessEq => GreaterThan,
            GreaterEq => LessThan,
            _ => return None,
        })
    }

    fn inner(term: &Rc<Term>) -> Option<(Operator, &[Rc<Term>])> {
        if let Some(Term::Op(op, args)) = term.remove_negation().map(Rc::as_ref) {
            if matches!(op, GreaterEq | LessEq | GreaterThan | LessThan | Equals) {
                return Some((*op, args));
            }
        } else if let Term::Op(op, args) = term.as_ref() {
            return Some((negate_operator(*op)?, args));
        }
        None
    }

    let (op, args) =
        inner(term).ok_or_else(|| LinearArithmeticError::InvalidDisequalityOp(term.clone()))?;

    match args {
        [a, b] => Ok((op, LinearComb::from_term(a), LinearComb::from_term(b))),
        _ => Err(LinearArithmeticError::TooManyArgsInDisequality(term.clone()).into()),
    }
}

/// A linear combination, represented by a hash map from non-constant terms to their coefficients,
/// plus a constant term. This is also used to represent a disequality, in which case the left side
/// is the non-constant terms and their coefficients, and the right side is the constant term. The
/// methods that construct and manipulate `LinearComb`s use the operations implemented by the
/// `RawOps` trait, and therefore don't reduce the fractions in `BigRational`s. This may lead to
/// errors when using methods that assume these fractions were reduced.
#[derive(Debug)]
pub struct LinearComb(
    pub(crate) AHashMap<Rc<Term>, BigRational>,
    pub(crate) BigRational,
);

impl LinearComb {
    fn new() -> Self {
        Self(AHashMap::new(), BigRational::zero())
    }

    /// Flattens a term and adds it to the linear combination, multiplying by the coefficient
    /// `coeff`. This method is only intended to be used in `LinearComb::from_term`.
    fn add_term(&mut self, term: &Rc<Term>, coeff: &BigRational) {
        // A note on performance: this function traverses the term recursively without making use
        // of a cache, which means sometimes it has to recompute the result for the same term more
        // than once. However, an old implementation of this method that could use a cache showed
        // that making use of one can actually make the performance of this function worse.
        // Benchmarks showed that it would more than double the average time of the "la_generic"
        // rule, which makes extensive use of `LinerComb`s. Because of that, we prefer to not use
        // a cache here, and traverse the term naively.

        match term.as_ref() {
            Term::Op(Operator::Add, args) => {
                for a in args {
                    self.add_term(a, coeff)
                }
            }
            Term::Op(Operator::Sub, args) if args.len() == 1 => {
                self.add_term(&args[0], &-coeff);
            }
            Term::Op(Operator::Sub, args) => {
                self.add_term(&args[0], coeff);
                for a in &args[1..] {
                    self.add_term(a, &-coeff);
                }
            }
            Term::Op(Operator::Mult, args) if args.len() == 2 => {
                let (var, inner_coeff) = match (args[0].as_fraction(), args[1].as_fraction()) {
                    (None, Some(coeff)) => (&args[0], coeff),
                    (Some(coeff), _) => (&args[1], coeff),
                    (None, None) => return self.insert(term.clone(), coeff.clone()),
                };
                self.add_term(var, &coeff.raw_mul(&inner_coeff));
            }
            _ => {
                if let Some(r) = term.as_fraction() {
                    self.1 = self.1.raw_add(&coeff.raw_mul(&r))
                } else {
                    self.insert(term.clone(), coeff.clone());
                }
            }
        }
    }

    /// Builds a linear combination from a term. Takes a term with nested additions, subtractions
    /// and multiplications, and flattens it to linear combination, calculating the coefficient of
    /// each atom.
    fn from_term(term: &Rc<Term>) -> Self {
        let mut result = Self::new();
        result.add_term(term, &BigRational::one());
        result
    }

    fn insert(&mut self, key: Rc<Term>, value: BigRational) {
        use std::collections::hash_map::Entry;

        match self.0.entry(key) {
            Entry::Occupied(mut e) => {
                let new_value = e.get().raw_add(&value);
                if new_value.is_zero() {
                    e.remove();
                } else {
                    e.insert(new_value);
                }
            }
            Entry::Vacant(e) => {
                e.insert(value);
            }
        }
    }

    fn add(mut self, other: Self) -> Self {
        for (var, coeff) in other.0 {
            self.insert(var, coeff)
        }
        self.1 = self.1.raw_add(&other.1);
        self
    }

    fn mul(&mut self, scalar: &BigRational) {
        if scalar.is_zero() {
            self.0.clear();
            self.1.set_zero();
            return;
        }

        if scalar.is_one() {
            return;
        }

        for coeff in self.0.values_mut() {
            *coeff = coeff.raw_mul(scalar);
        }
        self.1 = self.1.raw_mul(scalar);
    }

    fn neg(&mut self) {
        for coeff in self.0.values_mut() {
            // While cloning here seems bad, it is actually faster than the alternatives
            *coeff = -coeff.clone();
        }
        self.1 = -self.1.clone()
    }

    fn sub(self, mut other: Self) -> Self {
        other.neg();
        self.add(other)
    }

    /// Finds the greatest common divisor of the coefficients in the linear combination. Returns
    /// `None` if the linear combination is empty, or if any of the coefficients is not an integer.
    fn coefficients_gcd(&self) -> Option<BigInt> {
        if !self.1.is_integer() {
            return None;
        }
        let coefficients = self.0.iter().map(|(_, coeff)| {
            if coeff.is_integer() {
                Some(coeff.to_integer().abs())
            } else {
                None
            }
        });

        let mut result = self.1.to_integer().abs();
        for c in coefficients {
            result = num_integer::gcd(c?, result);
        }

        // If the linear combination is all zeros, the result would also be zero. In that case, we
        // have to return one instead
        Some(std::cmp::max(BigInt::one(), result))
    }
}

fn strengthen(op: Operator, disequality: &mut LinearComb, a: &BigRational) -> Operator {
    // Multiplications are expensive, so we avoid them if we can
    let is_integer = if a.is_zero() {
        true
    } else if a.is_one() {
        disequality.1.is_integer()
    } else {
        // This code is checking if `disequality.1 * a` is an integer, but `is_integer` assumes
        // that the `BigRational` is reduced. We instead directly check if the numerator is a
        // multiple of the denominator
        let constant = disequality.1.raw_mul(a);
        constant.numer().is_multiple_of(constant.denom())
    };

    match op {
        Operator::GreaterEq if is_integer => op,

        // In some cases, when the disequality is over integers, we can make the strengthening
        // rules even stronger. Consider for instance the following example:
        //     (step t1 (cl
        //         (not (<= (- 1) n))
        //         (not (<= (- 1) (+ n m)))
        //         (<= (- 2) (* 2 n))
        //         (not (<= m 1))
        //     ) :rule la_generic :args (1 1 1 1))
        // After the third disequality is negated and flipped, it becomes:
        //     -2 * n > 2
        // If nothing fancy is done, this would strenghten to:
        //     -2 * n >= 3
        // However, in this case, we can divide the disequality by 2 before strengthening, and then
        // multiply it by 2 to get back. This would result in:
        //     -2 * n > 2
        //     -1 * n > 1
        //     -1 * n >= 2
        //     -2 * n >= 4
        // This is a stronger statement, and follows from the original disequality. Importantly,
        // this strengthening is sometimes necessary to check some "la_generic" steps. To find the
        // value by which we should divide we have to take the greatest common divisor of the
        // coefficients (including the constant value on the right-hand side), as this makes sure
        // all coefficients will continue being integers after the division. This strengthening is
        // still valid because, since the variables are integers, the result of their linear
        // combination will always be a multiple of their GCD.
        Operator::GreaterThan if is_integer => {
            // Instead of dividing and then multiplying back, we just multiply the "+ 1"
            // that is added by the strengthening rule
            let increment = disequality.coefficients_gcd().unwrap_or_else(BigInt::one);
            disequality.1 = disequality.1.floor() + increment;
            Operator::GreaterEq
        }
        Operator::GreaterThan | Operator::GreaterEq => {
            disequality.1 = disequality.1.floor() + BigRational::one();
            Operator::GreaterEq
        }
        Operator::LessThan | Operator::LessEq => unreachable!(),
        _ => op,
    }
}

pub fn la_generic(RuleArgs { conclusion, args, .. }: RuleArgs) -> RuleResult {
    assert_num_args(args, conclusion.len())?;

    let args: Vec<_> = args
        .iter()
        .map(|a| match a {
            ProofArg::Term(a) => a
                .as_fraction()
                .ok_or_else(|| CheckerError::ExpectedAnyNumber(a.clone())),
            ProofArg::Assign(n, v) => Err(CheckerError::ExpectedTermStyleArg(n.clone(), v.clone())),
        })
        .collect::<Result<_, _>>()?;

    let final_disequality = conclusion
        .iter()
        .zip(args)
        .map(|(phi, a)| -> Result<_, CheckerError> {
            // Steps 1 and 2: Negate the disequality
            let (mut op, s1, s2) = negate_disequality(phi)?;

            // Step 3: Move all non constant terms to the left side, and the d terms to the right.
            // We move everything to the left side by subtracting s2 from s1
            let mut disequality = s1.sub(s2);
            disequality.1 = -disequality.1; // We negate d to move it to the other side

            // If the operator is < or <=, we flip the disequality so it is > or >=
            if op == Operator::LessThan {
                disequality.neg();
                op = Operator::GreaterThan;
            } else if op == Operator::LessEq {
                disequality.neg();
                op = Operator::GreaterEq;
            }

            // Step 4: Apply strengthening rules
            let op = strengthen(op, &mut disequality, &a);

            // Step 5: Multiply disequality by a
            let a = match op {
                Operator::Equals => a,
                _ => a.abs(),
            };
            disequality.mul(&a);

            Ok((op, disequality))
        })
        .try_fold(
            (Operator::Equals, LinearComb::new()),
            |(acc_op, acc), item| -> Result<_, CheckerError> {
                let (op, diseq) = item?;
                let new_acc = acc.add(diseq);
                let new_op = match (acc_op, op) {
                    (_, Operator::GreaterEq) => Operator::GreaterEq,
                    (Operator::Equals, Operator::GreaterThan) => Operator::GreaterThan,
                    _ => acc_op,
                };
                Ok((new_op, new_acc))
            },
        )?;

    let (op, LinearComb(left_side, right_side)) = &final_disequality;

    let is_disequality_true = {
        use std::cmp::Ordering;
        use Operator::*;

        // If the operator encompasses the actual relationship between 0 and the right side, the
        // disequality is true
        match BigRational::zero().cmp(right_side) {
            Ordering::Less => matches!(op, LessThan | LessEq),
            Ordering::Equal => matches!(op, LessEq | GreaterEq | Equals),
            Ordering::Greater => matches!(op, GreaterThan | GreaterEq),
        }
    };

    // The left side must be empty (that is, equal to 0), and the final disequality must be
    // contradictory
    rassert!(
        left_side.is_empty() && !is_disequality_true,
        LinearArithmeticError::DisequalityIsNotContradiction(*op, final_disequality.1),
    );
    Ok(())
}

pub fn lia_generic(_: RuleArgs) -> RuleResult {
    // The "lia_generic" rule is very similar to the "la_generic" rule, but the additional
    // arguments aren't given. In order to properly check this rule, the checker would need to
    // infer these arguments, which would be very complicated and slow. Therefore, for now, we just
    // ignore the rule and give a warning. Eventually, we plan to use cvc5 to help check this rule.
    // This would be done by constructing a problem in a format that cvc5 can solve, calling cvc5
    // with it, and parsing and checking the result proof.
    log::warn!("encountered \"lia_generic\" rule, ignoring");
    Ok(())
}

pub fn la_disequality(RuleArgs { conclusion, .. }: RuleArgs) -> RuleResult {
    assert_clause_len(conclusion, 1)?;

    let ((t1_1, t2_1), (t1_2, t2_2), (t2_3, t1_3)) = match_term_err!(
        (or (= t1 t2) (not (<= t1 t2)) (not (<= t2 t1))) = &conclusion[0]
    )?;
    assert_eq(t1_1, t1_2)?;
    assert_eq(t1_2, t1_3)?;
    assert_eq(t2_1, t2_2)?;
    assert_eq(t2_2, t2_3)
}

fn assert_less_than(a: &Rc<Term>, b: &Rc<Term>) -> RuleResult {
    rassert!(
        a.as_signed_number_err()? < b.as_signed_number_err()?,
        LinearArithmeticError::ExpectedLessThan(a.clone(), b.clone())
    );
    Ok(())
}

fn assert_less_eq(a: &Rc<Term>, b: &Rc<Term>) -> RuleResult {
    rassert!(
        a.as_signed_number_err()? <= b.as_signed_number_err()?,
        LinearArithmeticError::ExpectedLessEq(a.clone(), b.clone())
    );
    Ok(())
}

pub fn la_tautology(RuleArgs { conclusion, .. }: RuleArgs) -> RuleResult {
    assert_clause_len(conclusion, 1)?;

    if let Some((first, second)) = match_term!((or phi_1 phi_2) = conclusion[0]) {
        // If the conclusion if of the second form, there are 5 possible cases:
        if let (Some((s_1, d_1)), Some((s_2, d_2))) = (
            match_term!((not (<= s d1)) = first),
            match_term!((<= s d2) = second),
        ) {
            // First case
            assert_eq(s_1, s_2)?;
            assert_less_eq(d_1, d_2)
        } else if let (Some((s_1, d_1)), Some((s_2, d_2))) = (
            match_term!((<= s d1) = first),
            match_term!((not (<= s d2)) = second),
        ) {
            // Second case
            assert_eq(s_1, s_2)?;
            assert_eq(d_1, d_2)
        } else if let (Some((s_1, d_1)), Some((s_2, d_2))) = (
            match_term!((not (>= s d1)) = first),
            match_term!((>= s d2) = second),
        ) {
            // Third case
            assert_eq(s_1, s_2)?;
            assert_less_eq(d_2, d_1)
        } else if let (Some((s_1, d_1)), Some((s_2, d_2))) = (
            match_term!((>= s d1) = first),
            match_term!((not (>= s d2)) = second),
        ) {
            // Fourth case
            assert_eq(s_1, s_2)?;
            assert_eq(d_1, d_2)
        } else if let (Some((s_1, d_1)), Some((s_2, d_2))) = (
            match_term!((not (<= s d1)) = first),
            match_term!((not (>= s d2)) = second),
        ) {
            // Fifth case
            assert_eq(s_1, s_2)?;
            assert_less_than(d_1, d_2)
        } else {
            Err(LinearArithmeticError::NotValidTautologyCase(conclusion[0].clone()).into())
        }
    } else {
        // If the conclusion if of the first form, we apply steps 1 through 3 from "la_generic"

        // Steps 1 and 2: Negate the disequality
        let (mut op, s1, s2) = negate_disequality(&conclusion[0])?;

        // Step 3: Move all non constant terms to the left side, and the d terms to the right.
        let mut disequality = s1.sub(s2);
        disequality.1 = -disequality.1;

        // If the operator is < or <=, we flip the disequality so it is > or >=
        if op == Operator::LessThan {
            disequality.neg();
            op = Operator::GreaterThan;
        } else if op == Operator::LessEq {
            disequality.neg();
            op = Operator::GreaterEq;
        }

        // The final disequality should be tautological
        let is_disequality_true = disequality.0.is_empty()
            && (disequality.1.is_positive()
                || op == Operator::GreaterThan && disequality.1.is_zero());
        rassert!(
            is_disequality_true,
            LinearArithmeticError::DisequalityIsNotTautology(op, disequality),
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn la_rw_eq() {
        test_cases! {
            definitions = "
                (declare-fun a () Int)
                (declare-fun b () Int)
                (declare-fun x () Real)
                (declare-fun y () Real)
            ",
            "Simple working examples" {
                "(step t1 (cl (= (= a b) (and (<= a b) (<= b a)))) :rule la_rw_eq)": true,
                "(step t1 (cl (= (= x y) (and (<= x y) (<= y x)))) :rule la_rw_eq)": true,
            }
            "Clause term is not of the correct form" {
                "(step t1 (cl (= (= b a) (and (<= a b) (<= b a)))) :rule la_rw_eq)": false,
                "(step t1 (cl (= (= x y) (and (<= x y) (<= x y)))) :rule la_rw_eq)": false,
            }
        }
    }

    #[test]
    fn la_generic() {
        test_cases! {
            definitions = "
                (declare-fun a () Real)
                (declare-fun b () Real)
                (declare-fun c () Real)
                (declare-fun m () Int)
                (declare-fun n () Int)
            ",
            "Simple working examples" {
                "(step t1 (cl (> a 0.0) (<= a 0.0)) :rule la_generic :args (1.0 1.0))": true,
                "(step t1 (cl (>= a 0.0) (< a 0.0)) :rule la_generic :args (1.0 1.0))": true,
                "(step t1 (cl (<= 0.0 0.0)) :rule la_generic :args (1.0))": true,

                "(step t1 (cl (< (+ a b) 1.0) (> (+ a b) 0.0))
                    :rule la_generic :args (1.0 (- 1.0)))": true,

                "(step t1 (cl (<= (+ a (- b a)) b)) :rule la_generic :args (1.0))": true,

                "(step t1 (cl (not (<= (- a b) (- c 1.0))) (<= (+ 1.0 (- a c)) b))
                    :rule la_generic :args (1.0 1.0))": true,
            }
            "Empty clause" {
                "(step t1 (cl) :rule la_generic)": false,
            }
            "Wrong number of arguments" {
                "(step t1 (cl (>= a 0.0) (< a 0.0)) :rule la_generic :args (1.0 1.0 1.0))": false,
            }
            "Invalid argument term" {
                "(step t1 (cl (>= a 0.0) (< a 0.0)) :rule la_generic :args (1.0 b))": false,
            }
            "Clause term is not of the correct form" {
                "(step t1 (cl (ite (= a b) false true)) :rule la_generic :args (1.0))": false,
                "(step t1 (cl (= a 0.0) (< a 0.0)) :rule la_generic :args (1.0 1.0))": false,
            }
            "Negation of disequalities is satisfiable" {
                "(step t1 (cl (< 0.0 0.0)) :rule la_generic :args (1.0))": false,

                "(step t1 (cl (< (+ a b) 1.0) (> (+ a b c) 0.0))
                    :rule la_generic :args (1.0 (- 1.0)))": false,
            }
            "Edge case where the strengthening rules need to be stronger" {
                "(step t1 (cl
                    (not (<= (- 1) n))
                    (not (<= (- 1) (+ n m)))
                    (<= (- 2) (* 2 n))
                    (not (<= m 1))
                ) :rule la_generic :args (1 1 1 1))": true,
            }
        }
    }

    #[test]
    fn la_disequality() {
        test_cases! {
            definitions = "
                (declare-fun a () Int)
                (declare-fun b () Int)
                (declare-fun x () Real)
                (declare-fun y () Real)
            ",
            "Simple working examples" {
                "(step t1 (cl (or (= a b) (not (<= a b)) (not (<= b a))))
                    :rule la_disequality)": true,
                "(step t1 (cl (or (= x y) (not (<= x y)) (not (<= y x))))
                    :rule la_disequality)": true,
            }
            "Clause term is not of the correct form" {
                "(step t1 (cl (or (= b a) (not (<= a b)) (not (<= b a))))
                    :rule la_disequality)": false,
                "(step t1 (cl (or (= x y) (not (<= y x)) (not (<= y x))))
                    :rule la_disequality)": false,
            }
        }
    }

    #[test]
    fn la_tautology() {
        test_cases! {
            definitions = "
                (declare-fun n () Int)
                (declare-fun x () Real)
            ",
            "First form" {
                "(step t1 (cl (<= n (+ 1 n))) :rule la_tautology)": true,
                "(step t1 (cl (< (- n 1) n)) :rule la_tautology)": true,
                "(step t1 (cl (not (<= n (- n 1)))) :rule la_tautology)": true,
                "(step t1 (cl (< 0 (- (+ 1 n) n))) :rule la_tautology)": true,
                "(step t1 (cl (not (<= (+ 1 n) (- (+ 1 n) 1)))) :rule la_tautology)": true,
            }
            "Second form" {
                "(step t1 (cl (or (not (<= x 5.0)) (<= x 6.0))) :rule la_tautology)": true,

                "(step t1 (cl (or (<= x 6.0) (not (<= x 6.0)))) :rule la_tautology)": true,
                "(step t1 (cl (or (<= x 6.1) (not (<= x 6.0)))) :rule la_tautology)": false,

                "(step t1 (cl (or (not (>= x 6.0)) (>= x 5.0))) :rule la_tautology)": true,

                "(step t1 (cl (or (>= x 5.0) (not (>= x 5.0)))) :rule la_tautology)": true,
                "(step t1 (cl (or (>= x 5.0) (not (>= x 5.1)))) :rule la_tautology)": false,

                "(step t1 (cl (or (not (<= x 4.0)) (not (>= x 5.0)))) :rule la_tautology)": true,
                "(step t1 (cl (or (not (<= x 5.0)) (not (>= x 5.0)))) :rule la_tautology)": false,
            }
        }
    }
}

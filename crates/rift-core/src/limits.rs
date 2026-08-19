use std::fmt;

/// Exhausted bounded-loop budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetExhausted {
    limit: usize,
}

impl BudgetExhausted {
    crate::property! {
        /// Returns configured iteration limit.
        pub const fn limit(self) -> usize = self.limit;
    }
}

impl fmt::Display for BudgetExhausted {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "loop budget of {} iterations exhausted",
            self.limit
        )
    }
}

impl std::error::Error for BudgetExhausted {}

/// Explicit budget for input-driven iteration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoopBudget {
    limit: usize,
    remaining: usize,
}

impl LoopBudget {
    /// Constructs budget allowing at most `limit` body executions.
    #[must_use]
    pub const fn new(limit: usize) -> Self {
        Self {
            limit,
            remaining: limit,
        }
    }

    /// Charges one body execution.
    ///
    /// # Errors
    ///
    /// Returns [`BudgetExhausted`] after configured limit has been consumed.
    pub const fn consume(&mut self) -> Result<(), BudgetExhausted> {
        if self.remaining == 0 {
            return Err(BudgetExhausted { limit: self.limit });
        }
        self.remaining -= 1;
        Ok(())
    }

    /// Returns unconsumed body executions.
    #[must_use]
    pub const fn remaining(self) -> usize {
        self.remaining
    }
}

#[cfg(test)]
mod tests {
    use super::{BudgetExhausted, LoopBudget};
    use crate::bounded_for;
    use std::cell::Cell;

    #[test]
    fn boundary_is_charged_before_body() {
        let mut visited = Vec::new();
        let result = bounded_for!(item in 0..4, budget = LoopBudget::new(2), {
            visited.push(item);
        });

        assert_eq!(result, Err(BudgetExhausted { limit: 2 }));
        assert_eq!(visited, [0, 1]);
    }

    #[test]
    fn expressions_are_evaluated_once_and_control_flow_works() {
        let iterator_calls = Cell::new(0);
        let budget_calls = Cell::new(0);
        let mut visited = Vec::new();
        let result = bounded_for!(
            item in {
                iterator_calls.set(iterator_calls.get() + 1);
                0..5
            },
            budget = {
                budget_calls.set(budget_calls.get() + 1);
                LoopBudget::new(5)
            },
            {
                if item == 1 {
                    continue;
                }
                if item == 3 {
                    break;
                }
                visited.push(item);
            }
        );

        assert_eq!(result, Ok(()));
        assert_eq!(visited, [0, 2]);
        assert_eq!(iterator_calls.get(), 1);
        assert_eq!(budget_calls.get(), 1);
    }

    #[test]
    fn retained_budget_reports_state_and_exhaustion() {
        let mut budget = LoopBudget::new(1);

        assert_eq!(budget.remaining(), 1);
        assert_eq!(budget.consume(), Ok(()));
        assert_eq!(budget.remaining(), 0);
        let exhausted = budget.consume();
        assert_eq!(exhausted.map_err(BudgetExhausted::limit), Err(1));
        assert_eq!(
            BudgetExhausted { limit: 1 }.to_string(),
            "loop budget of 1 iterations exhausted"
        );
    }
}

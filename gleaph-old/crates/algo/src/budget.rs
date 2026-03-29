use gleaph_types::GleaphError;

pub trait InstructionBudget {
    fn remaining(&self) -> u64;
    fn consume(&mut self, cost: u64) -> Result<(), GleaphError>;
    fn check(&self) -> Result<(), GleaphError> {
        if self.remaining() == 0 {
            Err(GleaphError::BudgetExhausted)
        } else {
            Ok(())
        }
    }
}

#[derive(Default)]
pub struct UnlimitedBudget;

impl InstructionBudget for UnlimitedBudget {
    fn remaining(&self) -> u64 {
        u64::MAX
    }

    fn consume(&mut self, _cost: u64) -> Result<(), GleaphError> {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CountingBudget {
    pub limit: u64,
    pub used: u64,
}

impl CountingBudget {
    pub fn new(limit: u64) -> Self {
        Self { limit, used: 0 }
    }
}

impl InstructionBudget for CountingBudget {
    fn remaining(&self) -> u64 {
        self.limit.saturating_sub(self.used)
    }

    fn consume(&mut self, cost: u64) -> Result<(), GleaphError> {
        self.used = self.used.saturating_add(cost);
        if self.used > self.limit {
            Err(GleaphError::BudgetExhausted)
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IcBudget {
    pub soft_limit: u64,
    pub consumed: u64,
}

impl IcBudget {
    pub fn new(soft_limit: u64) -> Self {
        Self {
            soft_limit,
            consumed: 0,
        }
    }
}

impl InstructionBudget for IcBudget {
    fn remaining(&self) -> u64 {
        self.soft_limit.saturating_sub(self.consumed)
    }

    fn consume(&mut self, cost: u64) -> Result<(), GleaphError> {
        self.consumed = self.consumed.saturating_add(cost);
        if self.consumed > self.soft_limit {
            Err(GleaphError::BudgetExhausted)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counting_budget_exhausts() {
        let mut b = CountingBudget::new(3);
        assert!(b.consume(2).is_ok());
        assert!(b.consume(1).is_ok());
        assert!(matches!(b.consume(1), Err(GleaphError::BudgetExhausted)));
    }

    #[test]
    fn unlimited_budget_never_fails() {
        let mut b = UnlimitedBudget;
        for _ in 0..100 {
            b.consume(1_000_000).unwrap();
            b.check().unwrap();
        }
    }
}

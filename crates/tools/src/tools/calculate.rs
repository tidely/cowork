use std::fmt;

use rig::tool::{Tool, ToolContext, ToolExecutionError};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Basic arithmetic operations available to the agent.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Add,
    Subtract,
    Multiply,
    Divide,
}

#[derive(Debug, Deserialize)]
pub struct CalculateArgs {
    pub operation: Operation,
    pub a: f64,
    pub b: f64,
}

#[derive(Debug, Serialize)]
pub struct Calculation {
    pub result: f64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum CalculateError {
    NonFiniteOperand,
    DivisionByZero,
    NonFiniteResult,
}

impl fmt::Display for CalculateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonFiniteOperand => formatter.write_str("operands must be finite numbers"),
            Self::DivisionByZero => formatter.write_str("cannot divide by zero"),
            Self::NonFiniteResult => formatter.write_str("result is not a finite number"),
        }
    }
}

impl std::error::Error for CalculateError {}

/// Computes one arithmetic operation on two numbers without external side effects.
pub struct Calculate;

impl Tool for Calculate {
    const NAME: &'static str = "calculate";
    type Error = CalculateError;
    type Args = CalculateArgs;
    type Output = Calculation;

    fn description(&self) -> String {
        "Calculate the sum, difference, product, or quotient of two numbers. Use for arithmetic rather than estimating.".into()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "operation": {
                    "type": "string",
                    "enum": ["add", "subtract", "multiply", "divide"],
                    "description": "The arithmetic operation to perform"
                },
                "a": { "type": "number", "description": "The first operand" },
                "b": { "type": "number", "description": "The second operand" }
            },
            "required": ["operation", "a", "b"],
            "additionalProperties": false
        })
    }

    fn map_error(&self, error: Self::Error) -> ToolExecutionError {
        ToolExecutionError::invalid_args(error.to_string())
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        if !args.a.is_finite() || !args.b.is_finite() {
            return Err(CalculateError::NonFiniteOperand);
        }
        if matches!(args.operation, Operation::Divide) && args.b == 0.0 {
            return Err(CalculateError::DivisionByZero);
        }

        let result = match args.operation {
            Operation::Add => args.a + args.b,
            Operation::Subtract => args.a - args.b,
            Operation::Multiply => args.a * args.b,
            Operation::Divide => args.a / args.b,
        };
        if !result.is_finite() {
            return Err(CalculateError::NonFiniteResult);
        }
        Ok(Calculation { result })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn calculate(operation: Operation, a: f64, b: f64) -> Result<f64, CalculateError> {
        futures::executor::block_on(
            Calculate.call(&mut ToolContext::new(), CalculateArgs { operation, a, b }),
        )
        .map(|output| output.result)
    }

    #[test]
    fn computes_all_operations() {
        assert_eq!(calculate(Operation::Add, 2.5, 3.0), Ok(5.5));
        assert_eq!(calculate(Operation::Subtract, 2.5, 3.0), Ok(-0.5));
        assert_eq!(calculate(Operation::Multiply, -2.0, 3.0), Ok(-6.0));
        assert_eq!(calculate(Operation::Divide, 7.5, 2.0), Ok(3.75));
    }

    #[test]
    fn rejects_zero_divisor_and_non_finite_numbers() {
        assert_eq!(
            calculate(Operation::Divide, 2.0, -0.0),
            Err(CalculateError::DivisionByZero)
        );
        assert_eq!(
            calculate(Operation::Add, f64::NAN, 1.0),
            Err(CalculateError::NonFiniteOperand)
        );
        assert_eq!(
            calculate(Operation::Multiply, f64::MAX, 2.0),
            Err(CalculateError::NonFiniteResult)
        );
    }

    #[test]
    fn schema_and_argument_names_match() {
        let args: CalculateArgs = serde_json::from_value(json!({
            "operation": "divide", "a": 6, "b": 2
        }))
        .unwrap();
        assert_eq!(calculate(args.operation, args.a, args.b), Ok(3.0));
        assert!(
            serde_json::from_value::<CalculateArgs>(json!({
                "operation": "pow", "a": 2, "b": 3
            }))
            .is_err()
        );
        assert_eq!(
            Calculate.parameters()["required"],
            json!(["operation", "a", "b"])
        );
    }
}

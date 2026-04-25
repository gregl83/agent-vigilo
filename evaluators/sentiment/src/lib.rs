wit_bindgen::generate!({
    path: "../../wit/evaluator.wit",
    world: "evaluator-world",
});

use exports::app::evaluator::evaluator::{
    Guest,
    Input,
    Output,
};
use app::evaluator::types::Data;
use app::evaluator::executor;

struct Evaluator;

impl Guest for Evaluator {
    fn evaluate(input: Input) -> Result<Output, String> {
        executor::trace("sentiment evaluator started");
        executor::debug(&format!("db context: {}", input.context.db));

        // evaluator logic here
        let val = "sentiment-result".to_string();

        Ok(Output {
            data: Data { val },
        })
    }
}

export!(Evaluator);

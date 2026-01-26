const MAX_RETRIES: usize = 3;

#[derive(Debug, Clone)]
struct Config {
    name: String,
    threshold: i32,
}

trait Processor {
    fn process(&self, input: &[i32]) -> Vec<i32>;
}

struct Multiplier {
    factor: i32,
}

impl Multiplier {
    fn new(factor: i32) -> Self {
        Self { factor }
    }
}

impl Processor for Multiplier {
    fn process(&self, input: &[i32]) -> Vec<i32> {
        let mapper = |value: i32| value * self.factor;
        let mut output = Vec::new();
        for value in input {
            output.push(mapper(*value));
        }
        output
    }
}

fn collect_until(limit: i32) -> Vec<i32> {
    let mut values = Vec::new();
    let mut current = 0;
    while current < limit {
        values.push(current);
        current += 1;
    }
    values
}

#[test]
fn test_collect_until() {
    assert_eq!(collect_until(2), vec![0, 1]);
}

#[test]
fn test_multiplier_process() {
    let multiplier = Multiplier::new(2);
    assert_eq!(multiplier.process(&[2, 3]), vec![4, 6]);
}

fn main() {
    let config = Config {
        name: String::from("sample"),
        threshold: 4,
    };
    let values = collect_until(config.threshold);
    let processor = Multiplier::new(2);
    let processed = processor.process(&values);
    for attempt in 0..MAX_RETRIES {
        println!("attempt {}", attempt);
    }
    println!("{}: {:?}", config.name, processed);
}

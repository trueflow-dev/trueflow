const MAX_RETRIES = 3;

class Multiplier {
  constructor(factor) {
    this.factor = factor;
  }

  process(values) {
    const mapper = (value) => value * this.factor;
    const output = [];
    for (const value of values) {
      output.push(mapper(value));
    }
    return output;
  }
}

function collectUntil(limit) {
  const values = [];
  let current = 0;
  while (current < limit) {
    values.push(current);
    current += 1;
  }
  return values;
}

describe("collector", () => {
  it("collects values", () => {
    const values = collectUntil(2);
    if (values.length !== 2 || values[0] !== 0 || values[1] !== 1) {
      throw new Error("collectUntil failed");
    }
  });
});

function main() {
  const config = { name: "sample", threshold: 4 };
  const processor = new Multiplier(2);
  const values = collectUntil(config.threshold);
  const processed = processor.process(values);

  for (let attempt = 0; attempt < MAX_RETRIES; attempt += 1) {
    console.log(`attempt ${attempt}`);
  }

  console.log(`${config.name}:`, processed);
}

main();

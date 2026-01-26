const MAX_RETRIES = 3;

enum Mode {
  Fast,
  Safe,
}

interface Processor {
  process(values: number[]): number[];
}

interface Config {
  name: string;
  threshold: number;
  mode: Mode;
}

class Multiplier implements Processor {
  constructor(private readonly factor: number) {}

  process(values: number[]): number[] {
    const mapper = (value: number): number => value * this.factor;
    const output: number[] = [];
    for (const value of values) {
      output.push(mapper(value));
    }
    return output;
  }
}

function collectUntil(limit: number): number[] {
  const values: number[] = [];
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

function main(): void {
  const config: Config = {
    name: "sample",
    threshold: 4,
    mode: Mode.Fast,
  };
  const processor = new Multiplier(2);
  const values = collectUntil(config.threshold);
  const processed = processor.process(values);

  for (let attempt = 0; attempt < MAX_RETRIES; attempt += 1) {
    console.log(`attempt ${attempt}`);
  }

  switch (config.mode) {
    case Mode.Fast:
      console.log(`${config.name}:`, processed);
      break;
    case Mode.Safe:
      console.log(`${config.name} (safe):`, processed);
      break;
  }
}

main();

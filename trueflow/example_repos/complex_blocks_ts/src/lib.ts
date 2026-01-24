export interface Context {
  fetchWorld(): World;
}

export interface World {
  transform(input: number[]): number[];
}

export function processData(ctx: Context, data: number[]): number[] {
  const output: number[] = [];

  for (let i = 0; i < data.length; i += 4) {
    const chunk = data.slice(i, i + 4);
    if (chunk.every((value) => value === 0)) {
      continue;
    }
    output.push(...chunk);
  }

  // Now we need to do the crazy stuff.
  const world = ctx.fetchWorld();
  const transformed = world.transform(output);

  return transformed;
}

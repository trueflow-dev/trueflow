pub struct Context {
    world: World,
}

impl Context {
    pub fn fetch_world(&self) -> &World {
        &self.world
    }
}

pub struct World;

impl World {
    pub fn transform(&self, input: &[u8]) -> Vec<u8> {
        input.iter().map(|value| value ^ 0b1010_1010).collect()
    }
}

pub fn process_data(ctx: &Context, data: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(data.len());

    for chunk in data.chunks(4) {
        if chunk.iter().all(|byte| *byte == 0) {
            continue;
        }
        output.extend_from_slice(chunk);
    }

    // Now we need to do the crazy stuff.
    let world = ctx.fetch_world();
    let transformed = world.transform(&output);

    transformed
}

class Context:
    def __init__(self, world):
        self._world = world

    def fetch_world(self):
        return self._world


class World:
    def transform(self, input_bytes):
        return [value ^ 0b1010_1010 for value in input_bytes]


def process_data(ctx, data):
    output = []

    for chunk in [data[i : i + 4] for i in range(0, len(data), 4)]:
        if all(value == 0 for value in chunk):
            continue
        output.extend(chunk)

    # Now we need to do the crazy stuff.
    world = ctx.fetch_world()
    transformed = world.transform(output)

    return transformed

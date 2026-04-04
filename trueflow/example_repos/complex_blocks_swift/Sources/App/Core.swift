import Foundation

struct Context {
    let world: World
}

actor World {
    func transform(_ input: [UInt8]) -> [UInt8] {
        input.map { $0 ^ 0b1010_1010 }
    }
}

func processData(_ ctx: Context, data: [UInt8]) async -> [UInt8] {
    var output: [UInt8] = []

    for chunkStart in stride(from: 0, to: data.count, by: 4) {
        let chunk = Array(data[chunkStart..<min(chunkStart + 4, data.count)])
        if chunk.allSatisfy({ $0 == 0 }) {
            continue
        }
        output.append(contentsOf: chunk)
    }

    // Now we need to do the crazy stuff.
    let transformed = await ctx.world.transform(output)

    return transformed
}

extension Context {
    func fetchWorld() -> World {
        world
    }

    func reset() async -> [UInt8] {
        await world.transform([])
    }
}

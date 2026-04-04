import Foundation
import Testing
import XCTest

typealias Payload = [UInt8]

actor Transformer {
    func transform(_ input: Payload) -> Payload {
        input.map { $0 ^ 0b1010_1010 }
    }
}

@Test("swift-testing path")
func testProcessData() async throws {
    let transformer = Transformer()
    let result = await transformer.transform([1, 2, 3, 4])
    #expect(result.count == 4)
}

final class LegacyTransformerTests: XCTestCase {
    func testLegacyTransform() {
        XCTAssertEqual(2 + 2, 4)
    }
}

import Foundation
@testable import Util
import struct Darwin.C

public typealias Handler = (Int) -> Void

public let version = "1.0"
var counter = 0

public protocol Requestable {
    associatedtype Element
    var url: String { get }
    func perform()
    init()
    subscript(index: Int) -> Element { get }
}

open class Session {
    public static let defaultPort = 443
    public private(set) var label: String = ""
    private var secret = 0
    public var (first, second) = (1, 2)

    public init(label: String) {
        let local = helper()
        self.label = label + String(local)
    }

    deinit {}

    public func request() -> Point {
        perform()
        return Point(x: 1, y: 2)
    }

    func perform() {
        func inner() {}
        var items: [Int] = []
        items.append(1)
        inner()
    }

    public subscript(index: Int) -> Int { index }
}

public actor Worker {}

public struct Point: Equatable {
    public var x: Int
    public var y: Int

    public static func == (lhs: Point, rhs: Point) -> Bool { lhs.x == rhs.x }
    static func <~> (lhs: Point, rhs: Point) -> Point { lhs }
}

extension Point {
    func ext() {}
}

private extension Point {
    func hiddenExt() {}
}

public enum Method: String {
    case get, post
    case custom
}

public func helper() -> Int { 1 }

fileprivate func fileHelper() {}

@freestanding(expression)
public macro stringify<T>(_ value: T) -> (T, String) = #externalMacro(module: "M", type: "S")

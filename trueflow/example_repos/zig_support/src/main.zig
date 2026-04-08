const std = @import("std");
usingnamespace @import("support.zig");

const limit: usize = 8;
var global_counter: usize = 0;

pub const Mode = enum {
    fast,
    safe,

    pub fn isFast(self: Mode) bool {
        return self == .fast;
    }
};

const State = union(enum) {
    idle,
    running: i32,
};

pub const Accumulator = struct {
    total: i32 = 0,
    const Label = "acc";
    var seed: i32 = 1;

    pub const Snapshot = struct {
        total: i32,
    };

    pub fn init() Accumulator {
        return .{ .total = 0 };
    }

    pub fn add(self: *Accumulator, value: i32) void {
        if (value == 0) return;

        // track additions
        self.total += value;
    }

    test "add updates total" {
        var acc = Accumulator.init();
        acc.add(2);
        try std.testing.expectEqual(@as(i32, 2), acc.total);
    }
};

pub fn helper(value: i32) i32 {
    if (value > limit) {
        return value;
    }

    return value + 1;
}

test "helper increments" {
    try std.testing.expectEqual(@as(i32, 2), helper(1));
}

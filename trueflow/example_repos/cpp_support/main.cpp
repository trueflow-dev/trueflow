#include <algorithm>
#include <iostream>
#include <string>
#include <vector>

namespace demo {

using Count = int;

enum class Mode {
    Idle,
    Run,
};

class Worker {
public:
    explicit Worker(int factor) : factor_(factor) {}

    int process(int value) const {
        // adjust values before returning
        if (value > factor_) {
            return value * factor_;
        }

        return value + factor_;
    }

    int total() const {
        return factor_;
    }

private:
    int factor_;
};

template <typename T>
T clamp_min(T value, T floor) {
    if (value < floor) {
        return floor;
    }

    return value;
}

int test_process_worker() {
    Worker worker(2);
    return clamp_min(worker.process(3), 0);
}

}  // namespace demo

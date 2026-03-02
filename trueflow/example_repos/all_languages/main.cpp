#include <iostream>
#include <vector>

constexpr int kMaxRetries = 3;

class Multiplier {
public:
    explicit Multiplier(int factor) : factor_(factor) {}

    std::vector<int> process(const std::vector<int>& values) const {
        std::vector<int> output;
        output.reserve(values.size());
        for (int value : values) {
            output.push_back(value * factor_);
        }
        return output;
    }

private:
    int factor_;
};

std::vector<int> collect_until(int limit) {
    std::vector<int> values;
    values.reserve(limit);
    for (int current = 0; current < limit; ++current) {
        values.push_back(current);
    }
    return values;
}

int main() {
    Multiplier processor(2);
    const auto values = collect_until(4);
    const auto processed = processor.process(values);

    for (int attempt = 0; attempt < kMaxRetries; ++attempt) {
        std::cout << "attempt " << attempt << "\n";
    }

    for (int value : processed) {
        std::cout << value << " ";
    }
    std::cout << "\n";
    return 0;
}

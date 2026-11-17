// A file the extractor's facts are fixed for. Not compiled.
#include <algorithm>
#include <string>
#include <vector>

namespace geometry {

class Point {
public:
    Point(int x, int y) : x_(x), y_(y) {}
    Point() = default;
    ~Point() {}

    int norm() const;
    bool operator==(const Point &other) const {
        return x_ == other.x_ and y_ == other.y_;
    }
    virtual void draw() = 0;

private:
    int x_, y_;
};

int Point::norm() const {
    return std::abs(x_) + std::abs(y_);
}

namespace {
int helper(std::vector<int> values) {
    int total = 0;
    for (int v : values) {
        if (v > 0 || v < -10) {
            total += v;
        }
    }
    return total;
}
}  // namespace

}  // namespace geometry

template <typename T>
struct Box {
    T get() const;
    T value;
};

template <typename T>
T Box<T>::get() const {
    return value;
}

int parse(const std::string &text) {
    try {
        return std::stoi(text);
    } catch (const std::invalid_argument &) {
        return 0;
    } catch (...) {
        throw;
    }
}

int main() {
    std::vector<int> values{3, 1, 2};
    std::sort(values.begin(), values.end(), [](int a, int b) { return a < b; });
    auto twice = [&values] {
        return [](int v) { return v * 2; }(values.size());
    };
    return twice() + R"(raw)"[0];
}

TEST(Point, Norm) {
    auto check = [](int n) { return n >= 0; };
    EXPECT_TRUE(check(geometry::Point(1, 2).norm()));
}

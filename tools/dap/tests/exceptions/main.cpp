#include <cstdio>

int destroyed = 0;
struct Guard {
    int count;
    ~Guard() { destroyed += count; }
};
void leaf() { Guard guard{1}; throw 42; }
void middle() { Guard guard{2}; leaf(); }
int ordinary(int value) { return value + 5; }

int main() {
    int sentinel = 37;
    for (int attempt = 0; attempt < 3; ++attempt) {
        try { middle(); }
        catch (int value) {
            if (value != 42 || destroyed != (attempt + 1) * 3) return 1;
        }
    }
    int observed = destroyed;
    int continued = ordinary(sentinel);
    std::printf("CONTINUED %d %d\n", continued, observed);
    return continued == 42 && observed == 9 ? 0 : 2;
}

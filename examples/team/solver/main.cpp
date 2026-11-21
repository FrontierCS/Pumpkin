#include <algorithm>
#include <iostream>
#include <vector>

using Edges = std::vector<std::vector<int>>;
std::vector<int> core_numbers(int vertices, const Edges& edges);

int main() {
    int vertices, count;
    std::cin >> vertices >> count;
    Edges edges(count);
    for (auto& edge : edges) {
        int size;
        std::cin >> size;
        edge.resize(size);
        for (int& vertex : edge) std::cin >> vertex;
    }
    for (int number : core_numbers(vertices, edges)) std::cout << number << ' ';
    std::cout << '\n';
}

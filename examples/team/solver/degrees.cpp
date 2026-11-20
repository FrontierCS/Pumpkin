std::vector<int> core_numbers(int vertices, const Edges& edges) {
    std::vector<int> degrees(vertices);
    for (const auto& edge : edges) {
        for (int vertex : edge) ++degrees[vertex];
    }
    return degrees;
}

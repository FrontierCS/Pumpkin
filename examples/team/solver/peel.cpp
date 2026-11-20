std::vector<int> core_numbers(int vertices, const Edges& edges) {
    std::vector<std::vector<int>> incident(vertices);
    for (int e = 0; e < static_cast<int>(edges.size()); ++e) {
        for (int vertex : edges[e]) incident[vertex].push_back(e);
    }
    std::vector<int> degree(vertices), cores(vertices, -1);
    std::vector<bool> removed(edges.size());
    for (int v = 0; v < vertices; ++v) degree[v] = incident[v].size();

    int level = 0;
    for (int remaining = vertices; remaining > 0; --remaining) {
        int next = -1;
        for (int v = 0; v < vertices; ++v) {
            if (cores[v] < 0 && (next < 0 || degree[v] < degree[next])) next = v;
        }
        level = std::max(level, degree[next]);
        cores[next] = level;
        for (int e : incident[next]) {
            if (removed[e]) continue;
            removed[e] = true;
            for (int v : edges[e]) --degree[v];
        }
    }
    return cores;
}

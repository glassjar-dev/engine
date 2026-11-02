#include <string>
using namespace std;

class FriendGraph {
 public:
  void add(string name);

 private:
  string stored;
};

void FriendGraph::add(string name) {
  stored = name;
}

void addPeople(FriendGraph& graph) {
  string name = "Elizabeth";
  graph.add(name);
}

int main() {
  FriendGraph graph;
  addPeople(graph);
  return 0;
}

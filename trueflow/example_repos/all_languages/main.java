package demo;

import java.util.List;

public class Main {
  private final int multiplier;

  public Main(int multiplier) {
    this.multiplier = multiplier;
  }

  public int process(List<Integer> values) {
    int total = 0;
    for (int value : values) {
      if (value > 0) {
        total += value * multiplier;
      }
    }
    return total;
  }

  public static void main(String[] args) {
    Main app = new Main(2);
    System.out.println(app.process(List.of(1, 2, 3)));
  }
}

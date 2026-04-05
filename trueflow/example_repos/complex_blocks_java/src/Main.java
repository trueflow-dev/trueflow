package demo;

import java.util.List;

public class Main {
  private int normalize(int value) {
    return Math.max(value, 0);
  }

  public int processData(List<Integer> values) {
    int total = 0;

    for (int value : values) {
      if (value > 0) {
        total += normalize(value);
      }
    }

    // finalize the aggregate before returning
    total += values.size();

    return total;
  }
}

library dart_support.app;

import 'dart:math';
import 'package:meta/meta.dart';

const maxRetries = 3;
final String defaultName = 'worker';
int globalCounter = 0;

typedef NameFormatter = String Function(String name);

String summarize(List<int> values) {
  var total = 0;

  for (final value in values) {
    if (value > 0) {
      total += max(value, 1);
    } else if (value == 0) {
      total += 1;
    }
    globalCounter += 1;
  }

  return '$defaultName:$total';
}

mixin CounterSupport {
  int hits = 0;

  void markHit() {
    hits += 1;
  }
}

class Worker with CounterSupport {
  static const version = '1';
  final String name;
  int jobs = 0;

  Worker(this.name);

  void process(List<int> values) {
    for (final value in values) {
      if (value > 0) {
        jobs += value;
      }
    }

    markHit();
  }

  String get label => '$name:$jobs';

  set label(String next) {
    jobs = next.length;
  }
}

extension WorkerTools on Worker {
  bool get ready => jobs >= 0;

  void reset() {
    jobs = 0;
  }
}

enum Mode {
  fast,
  slow;

  String get label => name;
}

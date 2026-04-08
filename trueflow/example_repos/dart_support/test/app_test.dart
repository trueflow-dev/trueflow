import 'package:test/test.dart';

import '../lib/app.dart';

void main() {
  group('Worker', () {
    test('processes positive values', () {
      final worker = Worker('demo');

      worker.process([1, 2, 3]);

      expect(worker.jobs, 6);
    });
  });

  test('reset keeps worker ready', () {
    final worker = Worker('demo');
    worker.reset();

    expect(worker.ready, isTrue);
  });
}

@TestOn('vm')
void testStandaloneSummary() {
  expect(summarize([1, 0]), 'worker:2');
}

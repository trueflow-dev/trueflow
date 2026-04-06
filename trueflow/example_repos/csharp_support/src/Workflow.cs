using System;
using System.Collections.Generic;

namespace Demo.Workflow {
    public interface IGreeter {
        string Name { get; }
        GreetingResult BuildGreeting(string target);
    }

    public readonly record struct GreetingResult(string Message, int Parts);

    public enum WorkflowStatus {
        Idle,
        Running,
    }

    public readonly struct GreetingOptions {
        public GreetingOptions(string prefix, int repeatCount) {
            Prefix = prefix;
            RepeatCount = repeatCount;
        }

        public string Prefix { get; }
        public int RepeatCount { get; }
    }

    public class Greeter : IGreeter {
        private readonly List<string> history = new();

        public Greeter(string name) {
            Name = name;
            Status = WorkflowStatus.Idle;
            LastResult = new GreetingResult(string.Empty, 0);
        }

        public string Name { get; }
        public WorkflowStatus Status { get; private set; }
        public GreetingResult LastResult { get; private set; }

        public GreetingResult BuildGreeting(string target) {
            Status = WorkflowStatus.Running;

            var parts = new List<string>();
            var options = new GreetingOptions("Hello", 3);

            for (var index = 0; index < options.RepeatCount; index++) {
                var label = $"{options.Prefix} {target}";

                if (index % 2 == 0) {
                    parts.Add(label.ToUpperInvariant());
                    continue;
                }

                parts.Add(label.ToLowerInvariant());
            }

            // Capture the final output before storing it.
            var message = string.Join(" | ", parts);
            history.Add(message);

            if (history.Count > 5) {
                history.RemoveAt(0);
            }

            var summary = $"{Name}:{parts.Count}";
            if (summary.Length == 0) {
                throw new InvalidOperationException("summary should never be empty");
            }

            LastResult = new GreetingResult(message, parts.Count);
            Status = WorkflowStatus.Idle;

            return LastResult;
        }

        public IReadOnlyList<string> SnapshotHistory() {
            return history;
        }
    }
}

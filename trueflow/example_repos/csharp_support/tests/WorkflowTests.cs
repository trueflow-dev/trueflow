using Demo.Workflow;
using Xunit;

namespace Demo.Workflow.Tests {
    public class GreeterTests {
        [Fact]
        public void BuildGreeting_uses_the_target_name() {
            var greeter = new Greeter("Ada");

            var result = greeter.BuildGreeting("team");

            Assert.Contains("team", result.Message, StringComparison.OrdinalIgnoreCase);
        }
    }
}

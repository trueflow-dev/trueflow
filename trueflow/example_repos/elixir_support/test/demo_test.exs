defmodule Demo.WorkerTest do
  use ExUnit.Case, async: true
  alias Demo.Worker

  describe "run/1" do
    test "keeps positive doubled values" do
      assert Worker.run([-1, 1, 2]) == {2, [2, 4]}
    end
  end

  defp helper(value) do
    value
  end
end

package demo.scala_support {
  import org.scalatest.funsuite.AnyFunSuite

  class WorkflowSuite extends AnyFunSuite {
    test("worker process returns adjusted sum") {
      val worker = Worker("demo", 2)

      assert(worker.process(List(1, 2, 3)) == 8)
    }

    test("normalize handles negatives") {
      assert(normalize(List(-1, 2, 3)) == 8)
    }
  }
}

package demo.scala_support {
  import scala.math.abs

  val defaultRetries = 3
  var globalMode = "normal"

  def normalize(values: List[Int]): Int = {
    val positives = values.filter(_ > 0)

    positives.map(abs).sum + defaultRetries
  }

  object Registry {
    val version = "1.0"

    given intOrdering: Ordering[Int] with
      def compare(left: Int, right: Int): Int = {
        left.compare(right)
      }

    def load(id: String): Worker = {
      Worker(id, defaultRetries)
    }
  }

  trait WorkerLike {
    def process(values: List[Int]): Int
    val label: String
  }

  class Worker(name: String, retries: Int) extends WorkerLike {
    val label: String = name
    var calls = 0

    def process(values: List[Int]): Int = {
      calls += 1

      val normalized = values.map(abs)
      normalized.sum + retries
    }
  }

  enum Mode {
    case Fast, Safe
    case Custom(name: String)
  }

  given defaultWorker: WorkerLike with
    val label: String = "default"

    def process(values: List[Int]): Int = {
      values.sum
    }
}

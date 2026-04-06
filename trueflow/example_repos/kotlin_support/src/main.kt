package demo.kotlin

import kotlin.math.abs
import kotlin.test.Test

const val DEFAULT_SCALE = 3
var globalCounter = 0

fun runScenario(values: List<Int>): Int {
    var total = 0

    // normalize and accumulate
    for (value in values) {
        if (value > 0) {
            total += value
        } else if (value < 0) {
            total += abs(value)
        }
        globalCounter += 1
    }

    if (values.isEmpty()) {
        return DEFAULT_SCALE
    }

    total += 1
    total += 2
    total += 3
    total += 4
    total += 5
    total += 6
    total += 7
    total += 8
    total += 9
    total += 10
    total += 11
    total += 12
    total += 13
    total += 14
    total += 15

    return total
}

interface WorkerPort {
    fun load(id: String): Worker
}

class Worker(private val scale: Int) : WorkerPort {
    val name = "worker"
    var enabled = true
    val retries = 2
    var attempts = 0

    override fun load(id: String): Worker {
        attempts += 1
        return Worker(id.length + scale)
    }

    fun process(values: List<Int>): Int {
        var total = 0
        for (value in values) {
            if (value > 0) {
                total += value * scale
            }
        }
        return total
    }

    fun describe(): String {
        return "$name:$enabled:$scale"
    }

    fun reset() {
        attempts = 0
        enabled = true
    }

    fun disable() {
        enabled = false
    }

    fun status(): String {
        return if (enabled) {
            "ready"
        } else {
            "disabled"
        }
    }
}

object Registry {
    val defaultWorker = Worker(DEFAULT_SCALE)
}

enum class Mode {
    FAST,
    SAFE,
}

@Test
fun testWorkerProcessing() {
    check(Worker(DEFAULT_SCALE).process(listOf(1, 2, 3)) == 18)
}

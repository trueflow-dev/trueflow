#include <assert.h>
#include <stdio.h>

typedef struct Worker {
    int id;
    int retries;
    int offset;
    int scale;
    int flags;
    int spare;
    int cache;
    int total;
    int errors;
    int warnings;
} Worker;

enum Mode {
    MODE_IDLE = 0,
    MODE_RUN = 1,
};

union Payload {
    int as_int;
    float as_float;
};

static int worker_limit = 3;
int declared_total(Worker worker);

int process_worker(Worker worker) {
    int total = worker.id;

    total += worker.retries;
    total += worker.offset;
    total += worker.scale;
    total += worker.flags;
    total += worker.spare;
    total += worker.cache;
    total += worker.total;

    // adjust noisy values
    total += worker.errors;
    total += worker.warnings;
    total += worker.id;
    total += worker.retries;
    total += worker.offset;
    total += worker.scale;
    total += worker.flags;
    total += worker.spare;

    if (total > worker_limit) {
        total -= worker_limit;
    }

    total += worker.cache;
    total += worker.total;
    total += worker.errors;
    total += worker.warnings;
    total += worker.id;
    total += worker.retries;
    total += worker.offset;
    total += worker.scale;

    return total;
}

void test_process_worker(void) {
    Worker worker = {0};
    assert(process_worker(worker) == 0);
}

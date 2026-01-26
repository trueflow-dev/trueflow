set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

project_name := "all_languages"
max_retries := 3

default: build

build:
    @echo "building {{project_name}}"

loop:
    @for i in $(seq 0 $(({{max_retries}} - 1))); do \
        echo "attempt $$i"; \
    done

greet name="sample":
    @echo "hello {{name}}"

process values:
    @for value in {{values}}; do \
        echo "$$((value * 2))"; \
    done

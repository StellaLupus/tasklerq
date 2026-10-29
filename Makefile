SHELL := /bin/bash
.DEFAULT_GOAL := help

.PHONY: help dev test lint fmt check migrate seed build deps clean

help: ## Показать список команд
	@grep -E '^[a-zA-Z0-9_-]+:.*?## .*$$' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "  %-12s %s\n", $$1, $$2}'

dev: ## Запустить сервис локально (нужен .env)
	cargo run --locked -- serve

test: ## Запустить все тесты
	cargo test --all-features --locked

lint: ## Проверить форматирование и clippy
	cargo fmt --all -- --check
	cargo clippy --all-targets --all-features -- -D warnings

fmt: ## Отформатировать код
	cargo fmt --all

check: ## Проверить сборку всех целей
	cargo check --all-targets --all-features --locked

migrate: ## Применить миграции PostgreSQL
	cargo run --locked -- migrate

seed: ## Заполнить базу seed-данными (идемпотентно)
	cargo run --locked -- seed

build: ## Собрать release-бинарник
	cargo build --release --locked

deps: ## Показать дубли критичных зависимостей
	cargo tree --duplicates

clean: ## Удалить артефакты сборки
	cargo clean

-- События безопасности: отказ LLM-валидации и превышение порога отказов
-- аутентификации webhook. Значения добавляются отдельно: использовать их в
-- этой же транзакции нельзя.

ALTER TYPE audit_event_type ADD VALUE IF NOT EXISTS 'llm_validation_failed';
ALTER TYPE audit_event_type ADD VALUE IF NOT EXISTS 'auth_failure_threshold_exceeded';

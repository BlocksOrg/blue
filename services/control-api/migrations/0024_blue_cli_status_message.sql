UPDATE public.client_status
SET status_reasons = (
    SELECT coalesce(jsonb_agg(
        CASE
            WHEN reason = 'Client report indicates local revision drift; run harness status for details.'
                THEN to_jsonb('Client report indicates local revision drift; run blue status for details.'::text)
            ELSE to_jsonb(reason)
        END
    ), '[]'::jsonb)
    FROM jsonb_array_elements_text(status_reasons) AS reasons(reason)
)
WHERE status_reasons @> '["Client report indicates local revision drift; run harness status for details."]'::jsonb;

CREATE OR REPLACE FUNCTION public.publish_governance_revision()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    PERFORM pg_notify(
        'governance_revision',
        json_build_object(
            'organization_id', NEW.organization_id,
            'revision', NEW.revision
        )::text
    );
    RETURN NEW;
END;
$$;

DROP TRIGGER IF EXISTS governance_revision_notify ON public.governance_config_revisions;

CREATE TRIGGER governance_revision_notify
AFTER INSERT ON public.governance_config_revisions
FOR EACH ROW
EXECUTE FUNCTION public.publish_governance_revision();

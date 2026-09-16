"""Only resources in the dedicated E2E account/bucket scope; fail closed on bad tags."""
import os
import time
import boto3


def handler(event, context):
    now = int(time.time())
    ec2, s3 = boto3.client("ec2"), boto3.client("s3")
    for page in ec2.get_paginator("describe_instances").paginate(Filters=[
        {"Name": "tag:BlueE2E", "Values": ["native"]},
        {"Name": "instance-state-name", "Values": ["pending", "running", "stopped", "stopping"]},
    ]):
        for reservation in page["Reservations"]:
            for instance in reservation["Instances"]:
                tags = {t["Key"]: t["Value"] for t in instance.get("Tags", [])}
                expiry = tags.get("ExpiresAt", "")
                if expiry.isdigit() and int(expiry) < now:
                    ec2.terminate_instances(InstanceIds=[instance["InstanceId"]])
                    print("terminated expired E2E lease", instance["InstanceId"])
    bucket = os.environ["BUCKET"]
    for page in s3.get_paginator("list_objects_v2").paginate(Bucket=bucket, Prefix="runs/"):
        expired = [{"Key": o["Key"]} for o in page.get("Contents", [])
                   if o["LastModified"].timestamp() < now - 14400]
        if expired:
            s3.delete_objects(Bucket=bucket, Delete={"Objects": expired, "Quiet": True})
    return {"status": "complete"}

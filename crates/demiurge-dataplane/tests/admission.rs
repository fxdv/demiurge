use demiurge_dataplane::{AdmitBucket, ShedReason};

// [DEMI-XDP-SHED] — bucket exhaustion sheds before L7.
#[test]
fn admit_bucket_sheds_when_exhausted() {
    let bucket = AdmitBucket::new(2);
    assert!(bucket.try_admit().is_ok());
    assert!(bucket.try_admit().is_ok());
    assert_eq!(bucket.try_admit(), Err(ShedReason::BucketExhausted));
    assert_eq!(bucket.shed_total(), 1);
    bucket.release(1);
    assert!(bucket.try_admit().is_ok());
}

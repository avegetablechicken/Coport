//! What Coport records about the traffic it forwards: the Activity Log and
//! its Request records, the Model calls read from their responses, the
//! request IDs Apps send, and the memory budget those observations share.
pub mod logger;
pub mod model_calls;
pub(crate) mod observation_memory;
pub(crate) mod request_ids;

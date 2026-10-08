//! Secondary escape estimation.
//!
//! Will hold variant H's SEE contexts: the adaptive escape-frequency
//! estimators indexed by context shape, used for non-binary contexts whose
//! symbols have all been masked, and the dummy SEE context for order -1.

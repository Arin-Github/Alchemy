use convert_case::Casing;
use juniper::meta::{Argument, Field};
use juniper::{
	Arguments, BoxFuture, ExecutionResult, IntoFieldError, Object, Registry, ScalarValue, Value, ID,
};
use rust_arango::{AqlQuery, ClientError};
use serde_json::{Map as JsonMap, Number as JsonNumber, Value as JsonValue};
use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::Arc;

use crate::api::schema::errors::NotFoundError;
use crate::api::schema::fields::Entity;
use crate::lib::database::api::{DbEntity, DbRelationship};
use crate::lib::database::aql::{
	AQLFilter, AQLOperation, AQLQuery, AQLQueryBind, AQLQueryParameter,
};
use crate::lib::database::DATABASE;

type FutureType<'b, S> = BoxFuture<'b, ExecutionResult<S>>;

pub struct OperationRegistry<S>
where
	S: ScalarValue + Send + Sync,
{
	operations: HashMap<String, OperationEntry<S>>,
}

pub struct OperationEntry<S>
where
	S: ScalarValue,
{
	pub closure: for<'a> fn(
		&'a OperationData<S>,
		&'a juniper::Arguments<S>,
		AQLQuery<'a>,
	) -> FutureType<'a, S>,
	pub arguments_closure: for<'a> fn(&mut Registry<'a, S>) -> Vec<Argument<'a, S>>,
	pub field_closure:
		for<'a> fn(&mut Registry<'a, S>, name: &str, data: &OperationData<S>) -> Field<'a, S>,

	pub data: Arc<OperationData<S>>,
}

impl<S> OperationRegistry<S>
where
	S: ScalarValue + Send + Sync,
{
	pub fn new() -> OperationRegistry<S> {
		OperationRegistry {
			operations: HashMap::new(),
		}
	}

	pub fn call_by_key<'b>(
		&'b self,
		key: &str,
		arguments: &'b Arguments<S>,
		query: AQLQuery<'b>,
	) -> Option<FutureType<'b, S>> {
		self.operations.get(key).map(|o| {
			let closure = o.closure;

			closure(&o.data, arguments, query)
		})
	}

	pub fn get_operations(&self) -> &HashMap<String, OperationEntry<S>> {
		&self.operations
	}

	pub fn get_operation(&self, key: &str) -> Option<&OperationEntry<S>> {
		self.operations.get(key)
	}

	pub fn register_entity(
		&mut self,
		entity: Arc<DbEntity>,
		relationships: Arc<Vec<DbRelationship>>,
	) {
		let data = Arc::new(OperationData {
			entity: entity.clone(),
			relationships: relationships.clone(),

			_phantom: Default::default(),
		});

		vec![
			self.register::<Get>(data.clone()),
			self.register::<GetAll>(data.clone()),
		];
	}

	fn register<T: 'static>(&mut self, data: Arc<OperationData<S>>) -> String
	where
		T: Operation<S>,
	{
		let k = T::get_operation_name(&data);

		self.operations.insert(
			k.clone(),
			OperationEntry {
				closure: T::call,
				arguments_closure: T::get_arguments,
				field_closure: T::build_field,
				data,
			},
		);

		k
	}
}

pub struct OperationData<S>
where
	S: ScalarValue,
{
	pub entity: Arc<DbEntity>,
	pub relationships: Arc<Vec<DbRelationship>>,

	_phantom: PhantomData<S>,
}

pub trait Operation<S>
where
	S: ScalarValue,
	Self: Send + Sync,
{
	fn call<'b>(
		data: &'b OperationData<S>,
		arguments: &'b Arguments<S>,
		query: AQLQuery<'b>,
	) -> FutureType<'b, S>;

	fn get_operation_name(data: &OperationData<S>) -> String;

	fn get_arguments<'r>(registry: &mut Registry<'r, S>) -> Vec<Argument<'r, S>>;

	fn build_field<'r>(
		registry: &mut Registry<'r, S>,
		name: &str,
		data: &OperationData<S>,
	) -> Field<'r, S>;

	fn get_relationship_edge_name(relationship: &DbRelationship) -> String {
		format!(
			"{}_{}",
			pluralizer::pluralize(
				relationship
					.from
					.name
					.to_case(convert_case::Case::Snake)
					.as_str(),
				1,
				false,
			),
			relationship.name
		)
	}
}

fn convert_number<S>(n: &JsonNumber) -> Value<S>
where
	S: ScalarValue + Send + Sync,
{
	return if n.is_i64() {
		let v = n.as_i64().unwrap();

		let res = if v > i32::MAX as i64 {
			i32::MAX
		} else if v < i32::MIN as i64 {
			i32::MIN
		} else {
			v as i32
		};

		Value::scalar(res)
	} else if n.is_u64() {
		let v = n.as_u64().unwrap();

		let res = if v > i32::MAX as u64 {
			i32::MAX
		} else if v < i32::MIN as u64 {
			i32::MIN
		} else {
			v as i32
		};

		Value::scalar(res)
	} else {
		let v = n.as_f64().unwrap();

		Value::scalar(v)
	};
}

fn convert_json_to_juniper_value<S>(data: &JsonMap<String, JsonValue>) -> Value<S>
where
	S: ScalarValue + Send + Sync,
{
	let mut object = Object::<S>::with_capacity(data.len());

	fn convert<S>(val: &JsonValue) -> Value<S>
	where
		S: ScalarValue + Send + Sync,
	{
		match val {
			JsonValue::Null => Value::null(),
			JsonValue::Bool(v) => Value::scalar(v.to_owned()),
			JsonValue::Number(n) => convert_number(n),
			JsonValue::String(s) => Value::scalar(s.to_owned()),
			JsonValue::Array(a) => Value::list(a.iter().map(|i| convert(i)).collect()),
			JsonValue::Object(ref o) => convert_json_to_juniper_value(o),
		}
	}

	for (key, val) in data {
		object.add_field(key, convert(val));
	}

	Value::Object(object)
}

pub struct Get;

impl<S> Operation<S> for Get
where
	S: ScalarValue + Send + Sync,
{
	fn call<'b>(
		data: &'b OperationData<S>,
		arguments: &'b Arguments<S>,
		mut query: AQLQuery<'b>,
	) -> FutureType<'b, S> {
		let time = std::time::Instant::now();

		let entity = &data.entity;
		let collection = &entity.collection_name;

		query.filter = Some(Box::new(AQLFilter {
			left_node: Box::new(AQLQueryParameter("_key".to_string())),
			operation: AQLOperation::EQUAL,
			right_node: Box::new(AQLQueryBind("id")),
		}));
		query.limit = Some(1);

		Box::pin(async move {
			let query_str = query.to_aql();

			println!("{}", &query_str);

			let entries_query = AqlQuery::builder()
				.query(&query_str)
				.bind_var("@collection".to_string(), collection.clone())
				.bind_var(
					query.get_argument_key("id"),
					arguments.get::<String>("id").unwrap(),
				);

			let entries: Result<Vec<JsonValue>, ClientError> = DATABASE
				.get()
				.await
				.database
				.aql_query(entries_query.build())
				.await;

			let not_found_error = NotFoundError::new(entity.name.clone()).into_field_error();

			println!("SQL: {:?}", time.elapsed());

			return match entries {
				Ok(data) => {
					if let Some(first) = data.first() {
						let time2 = std::time::Instant::now();

						let ret = Ok(convert_json_to_juniper_value(first.as_object().unwrap()));

						println!("Conversion: {:?}", time2.elapsed());

						return ret;
					}

					Err(not_found_error)
				}
				Err(e) => {
					println!("{:?}", e);

					Err(not_found_error)
				}
			};
		})
	}

	fn get_operation_name(data: &OperationData<S>) -> String {
		format!(
			"get{}",
			pluralizer::pluralize(
				data.entity
					.name
					.to_case(convert_case::Case::Pascal)
					.as_str(),
				1,
				false,
			)
		)
	}

	fn get_arguments<'r>(registry: &mut Registry<'r, S>) -> Vec<Argument<'r, S>> {
		vec![registry.arg::<ID>("id", &())]
	}

	fn build_field<'r>(
		registry: &mut Registry<'r, S>,
		name: &str,
		data: &OperationData<S>,
	) -> Field<'r, S> {
		registry.field::<Option<Entity>>(name, &data)
	}
}

pub struct GetAll;

impl<S> Operation<S> for GetAll
where
	S: ScalarValue + Send + Sync,
{
	fn call<'b>(
		data: &'b OperationData<S>,
		arguments: &'b Arguments<S>,
		mut query: AQLQuery<'b>,
	) -> FutureType<'b, S> {
		let time = std::time::Instant::now();

		let entity = &data.entity;
		let collection = &entity.collection_name;

		query.limit = arguments.get::<i32>("limit");

		Box::pin(async move {
			let query_str = query.to_aql();

			println!("{}", &query_str);

			let entries_query = AqlQuery::builder()
				.query(&query_str)
				.bind_var("@collection".to_string(), collection.clone());

			let entries: Result<Vec<JsonValue>, ClientError> = DATABASE
				.get()
				.await
				.database
				.aql_query(entries_query.build())
				.await;

			let not_found_error = NotFoundError::new(entity.name.clone()).into_field_error();

			println!("SQL: {:?}", time.elapsed());

			return match entries {
				Ok(data) => {
					let mut output = Vec::<Value<S>>::new();

					let time2 = std::time::Instant::now();

					for datum in data {
						output.push(convert_json_to_juniper_value(datum.as_object().unwrap()));
					}

					println!("Conversion: {:?}", time2.elapsed());

					Ok(Value::list(output))
				}
				Err(e) => {
					println!("{:?}", e);

					Err(not_found_error)
				}
			};
		})
	}

	fn get_operation_name(data: &OperationData<S>) -> String {
		format!(
			"getAll{}",
			pluralizer::pluralize(
				data.entity
					.name
					.to_case(convert_case::Case::Pascal)
					.as_str(),
				2,
				false,
			)
		)
	}

	fn get_arguments<'r>(registry: &mut Registry<'r, S>) -> Vec<Argument<'r, S>> {
		vec![
			registry.arg::<Option<i32>>("limit", &())
		]
	}

	fn build_field<'r>(
		registry: &mut Registry<'r, S>,
		name: &str,
		data: &OperationData<S>,
	) -> Field<'r, S> {
		registry.field::<Vec<Entity>>(name, &data)
	}
}

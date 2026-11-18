DOCKER_NAME=service-maintainer

if [ $# -eq 0 ];  then
  docker stack deploy --with-registry-auth -c docker-compose.yaml $DOCKER_NAME
elif [ "$1" = "remove" ]; then
  docker stack rm $DOCKER_NAME
else
  echo "Invalid argument: $1"
fi

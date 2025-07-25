package main

import (
	"context"
	"net"

	"github.com/chroma-core/chroma/go/pkg/leader"
	"github.com/chroma-core/chroma/go/pkg/log/configuration"
	"github.com/chroma-core/chroma/go/pkg/log/metrics"
	"github.com/chroma-core/chroma/go/pkg/log/purging"
	"github.com/chroma-core/chroma/go/pkg/log/repository"
	"github.com/chroma-core/chroma/go/pkg/log/server"
	"github.com/chroma-core/chroma/go/pkg/log/sysdb"
	"github.com/chroma-core/chroma/go/pkg/proto/logservicepb"
	"github.com/chroma-core/chroma/go/pkg/utils"
	libs "github.com/chroma-core/chroma/go/shared/libs"
	"github.com/chroma-core/chroma/go/shared/otel"
	sharedOtel "github.com/chroma-core/chroma/go/shared/otel"
	"github.com/pingcap/log"
	"github.com/rs/zerolog"
	"go.uber.org/automaxprocs/maxprocs"
	"go.uber.org/zap"
	"google.golang.org/grpc"
	"google.golang.org/grpc/health"
	healthgrpc "google.golang.org/grpc/health/grpc_health_v1"
)

// hard-coding this here despite it also being in pkg/grpcutils/service.go because
// using the methods from grpcutils results in breaking our metrics collection for
// some reason. This service is being deprecated soon, so this is just a quick fix.
const maxGrpcFrameSize = 256 * 1024 * 1024

func main() {
	ctx := context.Background()

	// Configure logger
	utils.LogLevel = zerolog.DebugLevel
	utils.ConfigureLogger()
	if _, err := maxprocs.Set(); err != nil {
		log.Fatal("can't set maxprocs", zap.Error(err))
	}
	log.Info("Starting log service")
	config := configuration.NewLogServiceConfiguration()
	err := otel.InitTracing(ctx, &otel.TracingConfig{
		Service:  "log-service",
		Endpoint: config.OPTL_TRACING_ENDPOINT,
	})
	if err != nil {
		log.Fatal("failed to initialize tracing", zap.Error(err))
	}
	conn, err := libs.NewPgConnection(ctx, config)
	if err != nil {
		log.Fatal("failed to connect to postgres", zap.Error(err))
	}
	sysDb := sysdb.NewSysDB(config.SYSDB_CONN)
	lr := repository.NewLogRepository(conn, sysDb)
	server := server.NewLogServer(lr)
	var listener net.Listener
	listener, err = net.Listen("tcp", ":"+config.PORT)
	if err != nil {
		log.Fatal("failed to listen", zap.Error(err))
	}
	s := grpc.NewServer(
		grpc.MaxRecvMsgSize(maxGrpcFrameSize),
		grpc.UnaryInterceptor(sharedOtel.ServerGrpcInterceptor),
	)
	healthcheck := health.NewServer()
	healthgrpc.RegisterHealthServer(s, healthcheck)

	logservicepb.RegisterLogServiceServer(s, server)
	log.Info("log service started", zap.String("address", listener.Addr().String()))
	go leader.AcquireLeaderLock(ctx, func(ctx context.Context) {
		go purging.PerformPurgingLoop(ctx, lr)
		go metrics.PerformMetricsLoop(ctx, lr)
	})
	if err := s.Serve(listener); err != nil {
		log.Fatal("failed to serve", zap.Error(err))
	}
}
